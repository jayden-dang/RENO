//! Behaviour tests for approval records: the life of a row, who decided, durability, fences, and
//! the repeat-denial lookup. `now` is a number the test owns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::Ordering;

use reno_proto::{
    ActionClass, ApprovalId, ApprovalOutcome, ApprovalStatus, ApprovalSubject, ExecutorId,
    ResolvedBy, RunId, Seq, TaskId,
};
use rusqlite::params;
use serde_json::json;
use tempfile::TempDir;

use crate::approvals::{ExpireScope, NewApproval};
use crate::error::DbResult;
use crate::writer::Durability;
use crate::{NewTask, Store, StoreError};

fn run_id(n: usize) -> RunId {
    RunId::from_suffix(&format!("r{n}")).unwrap()
}

fn appr(n: usize) -> ApprovalId {
    ApprovalId::from_suffix(&format!("a{n}")).unwrap()
}

fn subject(command: &str) -> ApprovalSubject {
    ApprovalSubject {
        action: ActionClass::Command,
        tool: "bash".into(),
        command: Some(command.into()),
        paths: Vec::new(),
        raw: json!({"secret": "token-123"}),
    }
}

fn new(n: usize, run: usize, command: &str) -> NewApproval {
    NewApproval {
        id: appr(n),
        run_id: run_id(run),
        request_id: format!("req{n}"),
        subject: subject(command),
        expires_at: 1_000_000,
    }
}

/// A store with `runs` claimed runs `r0..`, each at fence 1.
async fn claimed(runs: usize) -> (TempDir, Store) {
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

async fn events_of(store: &Store, kind: &str) -> Vec<crate::EventRecord> {
    store
        .events_since(Seq(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

fn durable(store: &Store) -> u64 {
    store.stats().durable_commits.load(Ordering::Relaxed)
}

#[tokio::test]
async fn a_new_request_is_pending_with_its_event_in_the_same_transaction() {
    let (_d, store) = claimed(1).await;
    store
        .create_approval(new(1, 0, "git push"), 1, 100)
        .await
        .unwrap();
    let row = store.get_approval(&appr(1)).await.unwrap().unwrap();
    assert_eq!(row.status, ApprovalStatus::Pending);
    assert_eq!(row.run_id, run_id(0));
    assert_eq!(row.request_id, "req1");
    assert_eq!(row.subject, subject("git push"));
    assert_eq!(row.subject_digest, subject("git push").digest());
    assert_eq!(
        (row.created_at, row.decided_at, row.expires_at),
        (100, None, 1_000_000)
    );
    assert_eq!(row.decided_by, None);
    assert_eq!(store.pending_approvals().await.unwrap(), vec![row]);

    let events = events_of(&store, "approval.requested").await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        (
            events[0].subject_type.as_str(),
            events[0].subject_id.as_str()
        ),
        ("approval", "appr_a1")
    );
    assert_eq!(events[0].ts, 100);
    // The payload carries the digest and never the subject or the harness's raw request.
    let text = events[0].data.to_string();
    assert!(text.contains(&subject("git push").digest()));
    assert!(
        !text.contains("git push") && !text.contains("token-123"),
        "{text}"
    );
}

#[tokio::test]
async fn creating_is_a_normal_write_and_decisions_are_durable_writes() {
    let (_d, store) = claimed(1).await;
    let before = durable(&store);
    store.create_approval(new(1, 0, "a"), 1, 10).await.unwrap();
    assert_eq!(durable(&store), before, "a pending row is not a decision");

    store
        .resolve_approval(
            &appr(1),
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            1,
            20,
        )
        .await
        .unwrap();
    assert_eq!(durable(&store), before + 1);

    store
        .record_approval(
            new(2, 0, "b"),
            ApprovalOutcome::Denied,
            ResolvedBy::Rule { name: "r".into() },
            1,
            30,
        )
        .await
        .unwrap();
    assert_eq!(durable(&store), before + 2);

    store.create_approval(new(3, 0, "c"), 1, 40).await.unwrap();
    store.expire_pending(ExpireScope::All, 50).await.unwrap();
    assert_eq!(durable(&store), before + 3);
}

#[tokio::test]
async fn a_human_decision_updates_the_row_and_writes_its_event_once() {
    let (_d, store) = claimed(1).await;
    store
        .create_approval(new(1, 0, "cargo test"), 1, 10)
        .await
        .unwrap();
    store
        .resolve_approval(
            &appr(1),
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            1,
            20,
        )
        .await
        .unwrap();
    let row = store.get_approval(&appr(1)).await.unwrap().unwrap();
    assert_eq!(row.status, ApprovalStatus::Approved);
    assert_eq!(row.decided_by, Some(ResolvedBy::Human));
    assert_eq!(row.decided_at, Some(20));
    assert!(store.pending_approvals().await.unwrap().is_empty());

    let events = events_of(&store, "approval.decided").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].principal.as_deref(), Some("human"));
    assert_eq!(events[0].data["status"], "approved");
    assert_eq!(events[0].data["decidedBy"], json!({"type": "human"}));

    // A decision is made once.
    let again = store
        .resolve_approval(&appr(1), ApprovalOutcome::Denied, ResolvedBy::Human, 1, 30)
        .await;
    assert!(
        matches!(again, Err(StoreError::NotFound { .. })),
        "{again:?}"
    );
    let missing = store
        .resolve_approval(&appr(9), ApprovalOutcome::Denied, ResolvedBy::Human, 1, 30)
        .await;
    assert!(
        matches!(missing, Err(StoreError::NotFound { .. })),
        "{missing:?}"
    );
    assert_eq!(
        store.get_approval(&appr(1)).await.unwrap().unwrap().status,
        ApprovalStatus::Approved
    );
    assert_eq!(events_of(&store, "approval.decided").await.len(), 1);
}

#[tokio::test]
async fn a_rule_decision_is_stored_decided_with_the_rules_name() {
    let (_d, store) = claimed(1).await;
    let by = ResolvedBy::Rule {
        name: "cargo in the worktree".into(),
    };
    store
        .record_approval(
            new(1, 0, "cargo test"),
            ApprovalOutcome::Approved,
            by.clone(),
            1,
            10,
        )
        .await
        .unwrap();
    let row = store.get_approval(&appr(1)).await.unwrap().unwrap();
    assert_eq!(
        (row.status, row.decided_by.clone()),
        (ApprovalStatus::Approved, Some(by))
    );
    assert_eq!(row.decided_at, Some(10));
    assert!(
        store.pending_approvals().await.unwrap().is_empty(),
        "an auto decision is never pending"
    );
    assert!(events_of(&store, "approval.requested").await.is_empty());
    let events = events_of(&store, "approval.decided").await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].principal.as_deref(),
        Some("rule:cargo in the worktree")
    );
}

#[tokio::test]
async fn an_expiry_is_status_expired_and_is_not_a_denial_for_the_repeat_rule() {
    let (_d, store) = claimed(1).await;
    store.create_approval(new(1, 0, "x"), 1, 10).await.unwrap();
    store
        .resolve_approval(
            &appr(1),
            ApprovalOutcome::Denied,
            ResolvedBy::Expired,
            1,
            20,
        )
        .await
        .unwrap();
    let row = store.get_approval(&appr(1)).await.unwrap().unwrap();
    assert_eq!(row.status, ApprovalStatus::Expired);
    assert_eq!(row.decided_by, Some(ResolvedBy::Expired));
    assert!(
        !store
            .was_denied(&run_id(0), &subject("x").digest())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn was_denied_is_per_run_and_per_digest_and_counts_every_kind_of_denial() {
    let (_d, store) = claimed(2).await;
    let digest = |c: &str| subject(c).digest();
    assert!(!store.was_denied(&run_id(0), &digest("a")).await.unwrap());
    // Human denial.
    store.create_approval(new(1, 0, "a"), 1, 10).await.unwrap();
    store
        .resolve_approval(&appr(1), ApprovalOutcome::Denied, ResolvedBy::Human, 1, 11)
        .await
        .unwrap();
    // Rule denial.
    store
        .record_approval(
            new(2, 0, "b"),
            ApprovalOutcome::Denied,
            ResolvedBy::Rule { name: "r".into() },
            1,
            12,
        )
        .await
        .unwrap();
    // A repeat is itself a denial.
    store
        .record_approval(
            new(3, 0, "a"),
            ApprovalOutcome::Denied,
            ResolvedBy::RepeatDenial,
            1,
            13,
        )
        .await
        .unwrap();
    // An approval is not.
    store.create_approval(new(4, 0, "c"), 1, 14).await.unwrap();
    store
        .resolve_approval(
            &appr(4),
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            1,
            15,
        )
        .await
        .unwrap();
    assert!(store.was_denied(&run_id(0), &digest("a")).await.unwrap());
    assert!(store.was_denied(&run_id(0), &digest("b")).await.unwrap());
    assert!(!store.was_denied(&run_id(0), &digest("c")).await.unwrap());
    assert!(!store.was_denied(&run_id(0), &digest("d")).await.unwrap());
    assert!(
        !store.was_denied(&run_id(1), &digest("a")).await.unwrap(),
        "a denial is remembered for its own run only"
    );
}

#[tokio::test]
async fn expire_pending_closes_all_or_one_runs_requests_and_reports_them() {
    let (_d, store) = claimed(2).await;
    store.create_approval(new(1, 0, "a"), 1, 10).await.unwrap();
    store.create_approval(new(2, 1, "b"), 1, 11).await.unwrap();
    store.create_approval(new(3, 0, "c"), 1, 12).await.unwrap();
    store
        .resolve_approval(
            &appr(3),
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            1,
            13,
        )
        .await
        .unwrap();

    let one = store
        .expire_pending(
            ExpireScope::Run {
                run: run_id(0),
                fence: 1,
            },
            20,
        )
        .await
        .unwrap();
    assert_eq!(
        one.iter()
            .map(|e| (e.id.clone(), e.request_id.as_str()))
            .collect::<Vec<_>>(),
        vec![(appr(1), "req1")],
        "only the pending request of that run; the approved one is left alone"
    );
    assert_eq!(store.pending_approvals().await.unwrap().len(), 1);

    let all = store.expire_pending(ExpireScope::All, 30).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        (all[0].id.clone(), all[0].run_id.clone()),
        (appr(2), run_id(1))
    );
    assert!(store.pending_approvals().await.unwrap().is_empty());
    assert!(
        store
            .expire_pending(ExpireScope::All, 40)
            .await
            .unwrap()
            .is_empty()
    );

    let decided = events_of(&store, "approval.decided").await;
    assert_eq!(
        decided.len(),
        3,
        "one event per row: the approval and the two expiries"
    );
    assert_eq!(
        decided
            .iter()
            .filter(|e| e.principal.as_deref() == Some("expired"))
            .count(),
        2
    );
}

#[tokio::test]
async fn a_stale_fence_is_refused_and_writes_nothing() {
    let (_d, store) = claimed(1).await;
    let stale = store.create_approval(new(1, 0, "a"), 7, 10).await;
    assert!(
        matches!(
            stale,
            Err(StoreError::StaleFence {
                supplied: 7,
                current: 1,
                ..
            })
        ),
        "{stale:?}"
    );
    assert!(store.get_approval(&appr(1)).await.unwrap().is_none());

    store.create_approval(new(1, 0, "a"), 1, 10).await.unwrap();
    let stale = store
        .resolve_approval(
            &appr(1),
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            2,
            20,
        )
        .await;
    assert!(
        matches!(stale, Err(StoreError::StaleFence { .. })),
        "{stale:?}"
    );
    let stale = store
        .record_approval(
            new(2, 0, "b"),
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            2,
            20,
        )
        .await;
    assert!(
        matches!(stale, Err(StoreError::StaleFence { .. })),
        "{stale:?}"
    );
    let stale = store
        .expire_pending(
            ExpireScope::Run {
                run: run_id(0),
                fence: 2,
            },
            20,
        )
        .await;
    assert!(
        matches!(stale, Err(StoreError::StaleFence { .. })),
        "{stale:?}"
    );
    assert_eq!(
        store.get_approval(&appr(1)).await.unwrap().unwrap().status,
        ApprovalStatus::Pending,
        "nothing changed"
    );
    assert!(store.get_approval(&appr(2)).await.unwrap().is_none());

    let unknown = store
        .create_approval(
            NewApproval {
                run_id: run_id(5),
                ..new(9, 0, "a")
            },
            1,
            10,
        )
        .await;
    assert!(
        matches!(unknown, Err(StoreError::NotFound { .. })),
        "{unknown:?}"
    );
}

#[tokio::test]
async fn a_reused_id_is_refused() {
    let (_d, store) = claimed(1).await;
    store.create_approval(new(1, 0, "a"), 1, 10).await.unwrap();
    let again = store.create_approval(new(1, 0, "b"), 1, 11).await;
    assert!(
        matches!(again, Err(StoreError::AlreadyExists { .. })),
        "{again:?}"
    );
    assert_eq!(events_of(&store, "approval.requested").await.len(), 1);
}

#[tokio::test]
async fn history_is_decided_rows_newest_first_filtered_by_run_and_limited() {
    let (_d, store) = claimed(2).await;
    store.create_approval(new(1, 0, "a"), 1, 10).await.unwrap();
    store.create_approval(new(2, 1, "b"), 1, 11).await.unwrap();
    store.create_approval(new(3, 0, "c"), 1, 12).await.unwrap(); // stays pending
    store
        .resolve_approval(
            &appr(2),
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            1,
            20,
        )
        .await
        .unwrap();
    store
        .resolve_approval(&appr(1), ApprovalOutcome::Denied, ResolvedBy::Human, 1, 21)
        .await
        .unwrap();
    store
        .record_approval(
            new(4, 0, "d"),
            ApprovalOutcome::Approved,
            ResolvedBy::Rule { name: "r".into() },
            1,
            22,
        )
        .await
        .unwrap();

    let ids = |rows: Vec<crate::StoredApproval>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(
        ids(store.approval_history(None, 10).await.unwrap()),
        vec![appr(4), appr(1), appr(2)],
        "newest decision first, and the pending row is not history"
    );
    assert_eq!(
        ids(store.approval_history(Some(&run_id(0)), 10).await.unwrap()),
        vec![appr(4), appr(1)]
    );
    assert_eq!(
        ids(store.approval_history(None, 2).await.unwrap()),
        vec![appr(4), appr(1)]
    );
    assert!(
        store
            .approval_history(Some(&run_id(1)), 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_decided_row_is_final_and_no_row_is_ever_deleted() {
    let (_d, store) = claimed(1).await;
    store.create_approval(new(1, 0, "a"), 1, 10).await.unwrap();
    store
        .resolve_approval(&appr(1), ApprovalOutcome::Denied, ResolvedBy::Human, 1, 20)
        .await
        .unwrap();
    let tamper = store
        .write(Durability::Normal, |c| {
            c.execute("UPDATE approvals SET status = 'approved'", [])
                .db()
        })
        .await;
    assert!(tamper.is_err(), "a decided approval cannot be flipped");
    let delete = store
        .write(Durability::Normal, |c| {
            c.execute("DELETE FROM approvals", []).db()
        })
        .await;
    assert!(delete.is_err(), "approval records are kept");
    assert_eq!(
        store.get_approval(&appr(1)).await.unwrap().unwrap().status,
        ApprovalStatus::Denied
    );
}

/// Inserts a row straight into the table, bypassing the typed API.
async fn raw_insert(
    store: &Store,
    id: String,
    status: &'static str,
    by: Option<&'static str>,
    rule: Option<&'static str>,
    decided_at: Option<i64>,
) -> Result<(), StoreError> {
    store
        .write(Durability::Normal, move |c| {
            c.execute(
                "INSERT INTO approvals (id, run_id, request_id, subject, subject_digest, status,
                     decided_by, rule, created_at, decided_at, expires_at)
                 VALUES (?1, 'run_r0', 'q', '{}', 'd', ?2, ?3, ?4, 1, ?5, 2)",
                params![id, status, by, rule, decided_at],
            )
            .db()
            .map(|_| ())
        })
        .await
}

#[tokio::test]
async fn the_table_refuses_rows_that_contradict_themselves() {
    let (_d, store) = claimed(1).await;
    for (n, (status, by, rule, at)) in [
        ("pending", None, None, None),
        ("approved", Some("human"), None, Some(5)),
        ("approved", Some("rule"), Some("r"), Some(5)),
        ("denied", Some("repeat_denial"), None, Some(5)),
        ("expired", Some("expired"), None, Some(5)),
    ]
    .into_iter()
    .enumerate()
    {
        let ok = raw_insert(&store, format!("appr_ok{n}"), status, by, rule, at).await;
        assert!(ok.is_ok(), "{status} {by:?}: {ok:?}");
    }
    for (n, (status, by, rule, at)) in [
        ("pending", Some("human"), None, None),
        ("pending", None, None, Some(5)),
        ("approved", None, None, Some(5)),
        ("approved", Some("human"), None, None),
        ("approved", Some("rule"), None, Some(5)),
        ("approved", Some("human"), Some("r"), Some(5)),
        ("expired", Some("human"), None, Some(5)),
        ("denied", Some("expired"), None, Some(5)),
        ("denied", Some("someone"), None, Some(5)),
        ("maybe", None, None, None),
    ]
    .into_iter()
    .enumerate()
    {
        let refused = raw_insert(&store, format!("appr_bad{n}"), status, by, rule, at).await;
        assert!(refused.is_err(), "{status} {by:?} {rule:?} {at:?}");
    }
}

#[tokio::test]
async fn a_row_needs_a_real_run() {
    let (_d, store) = claimed(1).await;
    let orphan = store
        .write(Durability::Normal, |c| {
            c.execute(
                "INSERT INTO approvals (id, run_id, request_id, subject, subject_digest, created_at, expires_at)
                 VALUES ('appr_o', 'run_nope', 'q', '{}', 'd', 1, 2)",
                [],
            )
            .db()
        })
        .await;
    assert!(orphan.is_err());
}
