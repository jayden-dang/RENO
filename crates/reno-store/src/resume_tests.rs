//! The store half of resume (T1.10): the session of a run, why it stopped, and queueing it again.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::{
    CancelledBy, ExecutorId, NotResumed, RunId, RunStatus, SessionRef, StopDetail, TaskId,
};
use tempfile::TempDir;

use crate::runs::Note;
use crate::{NewTask, Store, StoreError};

fn task() -> TaskId {
    TaskId::from_suffix("t0").unwrap()
}

fn run() -> RunId {
    RunId::from_suffix("r0").unwrap()
}

/// A store with one run claimed (fence 1) and running.
async fn running() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    store
        .create_task(
            NewTask {
                id: task(),
                goal: "g".into(),
                class: None,
                spec: serde_json::json!({}),
            },
            0,
        )
        .await
        .unwrap();
    let executor: ExecutorId = "claude".parse().unwrap();
    store
        .enqueue_run(&task(), &run(), &executor, 0)
        .await
        .unwrap();
    store.claim_next("a", 10, 100).await.unwrap().unwrap();
    store
        .transition(&run(), 1, RunStatus::Running, 20)
        .await
        .unwrap();
    (dir, store)
}

async fn record(store: &Store) -> crate::RunRecord {
    store.get_run(&run()).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_session_is_recorded_once_and_survives_the_run_ending() {
    let (_d, store) = running().await;
    assert_eq!(record(&store).await.session_ref, None);
    let session = SessionRef("sess-1".into());
    store.record_session(&run(), 1, &session, 30).await.unwrap();
    store.record_session(&run(), 1, &session, 31).await.unwrap();
    store
        .transition(&run(), 1, RunStatus::Interrupted, 40)
        .await
        .unwrap();
    assert_eq!(record(&store).await.session_ref.as_deref(), Some("sess-1"));
    let recorded = store
        .events_since(reno_proto::Seq(0), 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "run.session_recorded")
        .count();
    assert_eq!(recorded, 1, "an unchanged session writes no second event");
}

#[tokio::test]
async fn a_stale_fence_cannot_record_a_session() {
    let (_d, store) = running().await;
    let error = store
        .record_session(&run(), 7, &SessionRef("x".into()), 30)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::StaleFence { .. }), "{error:?}");
}

#[tokio::test]
async fn a_noted_transition_stores_why_the_run_stopped() {
    let (_d, store) = running().await;
    let stop = StopDetail::Cancelled {
        by: CancelledBy::User,
    };
    store
        .transition_noted(
            &run(),
            1,
            RunStatus::Interrupted,
            Note {
                stop: Some(stop.clone()),
                not_resumed: stop.forbids_auto_resume(),
            },
            40,
        )
        .await
        .unwrap();
    let row = record(&store).await;
    assert_eq!(row.stop, Some(stop));
    assert_eq!(row.not_resumed, Some(NotResumed::Cancelled));
}

#[tokio::test]
async fn resuming_queues_the_same_run_keeps_its_start_and_clears_the_reasons() {
    let (_d, store) = running().await;
    store
        .record_session(&run(), 1, &SessionRef("sess-1".into()), 30)
        .await
        .unwrap();
    store
        .transition_noted(
            &run(),
            1,
            RunStatus::Interrupted,
            Note {
                stop: Some(StopDetail::Crashed),
                not_resumed: Some(NotResumed::NoSession),
            },
            40,
        )
        .await
        .unwrap();

    let resumed = store
        .resume_run(&run(), 1, Some("go on".into()), 50)
        .await
        .unwrap();
    assert_eq!(resumed.from_attempt, 1);
    assert_eq!(resumed.session, SessionRef("sess-1".into()));
    let row = record(&store).await;
    assert_eq!(row.status, RunStatus::Queued);
    assert_eq!(row.started_at, Some(20), "one run, one start");
    assert_eq!((row.stop, row.not_resumed), (None, None));
    assert_eq!(row.resume_prompt.as_deref(), Some("go on"));
    assert_eq!(row.session_ref.as_deref(), Some("sess-1"));

    // The next claim is attempt 2: the fence is the attempt number.
    let claim = store.claim_next("a", 60, 100).await.unwrap().unwrap();
    assert_eq!((claim.run_id, claim.fence), (run(), 2));
    let attempts: i64 = store
        .read(|c| {
            c.query_row("SELECT count(*) FROM attempts", [], |r| r.get(0))
                .map_err(crate::error::db)
        })
        .await
        .unwrap();
    assert_eq!(attempts, 2);
}

#[tokio::test]
async fn a_run_without_a_session_or_in_another_state_is_not_resumed_by_the_store() {
    let (_d, store) = running().await;
    // Running: no edge to queued.
    let error = store.resume_run(&run(), 1, None, 30).await.unwrap_err();
    assert!(
        matches!(error, StoreError::IllegalTransition { .. }),
        "{error:?}"
    );
    store
        .transition(&run(), 1, RunStatus::Interrupted, 40)
        .await
        .unwrap();
    let error = store.resume_run(&run(), 1, None, 50).await.unwrap_err();
    assert!(matches!(error, StoreError::InvalidArgument(_)), "{error:?}");
    assert_eq!(record(&store).await.status, RunStatus::Interrupted);
}

#[tokio::test]
async fn a_failed_run_with_a_session_can_be_resumed() {
    let (_d, store) = running().await;
    store
        .record_session(&run(), 1, &SessionRef("sess-1".into()), 30)
        .await
        .unwrap();
    store
        .transition(&run(), 1, RunStatus::Evaluating, 40)
        .await
        .unwrap();
    store
        .transition(&run(), 1, RunStatus::Failed, 50)
        .await
        .unwrap();
    store.resume_run(&run(), 1, None, 60).await.unwrap();
    let row = record(&store).await;
    assert_eq!((row.status, row.ended_at), (RunStatus::Queued, None));
}

#[tokio::test]
async fn not_resumed_is_recorded_on_the_interrupted_run() {
    let (_d, store) = running().await;
    store
        .transition(&run(), 1, RunStatus::Interrupted, 40)
        .await
        .unwrap();
    let why = NotResumed::AttemptsExhausted {
        attempts: 3,
        max: 3,
    };
    store.set_not_resumed(&run(), 1, &why, 50).await.unwrap();
    assert_eq!(record(&store).await.not_resumed, Some(why));
}

#[tokio::test]
async fn a_model_switch_is_the_runs_model_and_the_next_attempts() {
    let (_d, store) = running().await;
    assert_eq!(record(&store).await.model, None);
    store
        .record_model(&run(), 1, "claude-sonnet-5-5")
        .await
        .unwrap();
    let row = record(&store).await;
    assert_eq!(row.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(row.spec["model"], "claude-sonnet-5-5");
    // A stale attempt cannot change it.
    assert!(
        store
            .record_model(&run(), 7, "claude-opus-5-5")
            .await
            .is_err()
    );
    assert_eq!(
        record(&store).await.model.as_deref(),
        Some("claude-sonnet-5-5")
    );
}
