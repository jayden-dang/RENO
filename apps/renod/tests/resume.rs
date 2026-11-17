//! `run.resume` through the real client against the daemon served in-process: a person continues
//! a run the daemon left alone, and a run that cannot be resumed is refused with a typed reason.
//! No network, no real harness (ARCH-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Fixture, is_done, within};
use futures::StreamExt;
use reno_client::ClientError;
use reno_proto::{
    CancelledBy, ErrorCode, ExecEvent, NotResumable, NotResumed, RunId, RunStatus, SessionRef,
    StopDetail, StopReason,
};

macro_rules! daemon_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() $body
    };
}

/// The typed reason a `run.resume` was refused with.
fn refused_for(error: ClientError) -> NotResumed {
    assert_eq!(error.code(), Some(ErrorCode::Conflict), "{error:?}");
    let ClientError::Remote(object) = error else {
        panic!("not a daemon error");
    };
    let data: NotResumable = serde_json::from_value(object.data.expect("typed data")).unwrap();
    data.why
}

/// `run.resume` once the run's supervisor has let go of it. A run's journal ends with its `Done`
/// before its slot is freed, so a resume sent the moment the stream ended can still be told
/// `StillStopping`; that is the daemon being right, and the answer to wait out.
async fn resume_when_stopped(
    fx: &Fixture,
    run: &RunId,
    prompt: Option<String>,
) -> Result<u32, ClientError> {
    loop {
        match within(fx.client.resume_run(run, prompt.clone())).await {
            Err(error) if is_still_stopping(&error) => tokio::task::yield_now().await,
            other => return other,
        }
    }
}

fn is_still_stopping(error: &ClientError) -> bool {
    let ClientError::Remote(object) = error else {
        return false;
    };
    object
        .data
        .clone()
        .and_then(|data| serde_json::from_value::<NotResumable>(data).ok())
        .is_some_and(|data| data.why == NotResumed::StillStopping)
}

async fn texts_until_held(fx: &Fixture, run: &RunId, texts: usize) {
    let mut events = Box::pin(fx.client.run_events(run.clone(), None));
    let mut seen = 0;
    while seen < texts {
        let item = within(events.next()).await.expect("event").expect("ok");
        if matches!(item.event, ExecEvent::Text { .. }) {
            seen += 1;
        }
    }
}

daemon_test! {
    async fn a_person_resumes_a_run_the_kill_switch_stopped_and_it_continues_the_same_session() {
        let fx = Fixture::start().await;
        let run = fx.run("mock-hold", "wait for me").await;
        texts_until_held(&fx, &run, 1).await;

        within(fx.client.pause(None, true)).await.unwrap();
        // Wait for the journal to close: the run is `interrupted` and says who stopped it.
        let first = fx.events(&run, None).await;
        let Some(ExecEvent::Done { stop, reason, .. }) = first.last().map(|i| &i.event) else {
            panic!("no Done: {first:?}");
        };
        assert_eq!(*stop, StopReason::Interrupted);
        assert_eq!(*reason, Some(StopDetail::Cancelled { by: CancelledBy::Pause }));
        let summary = within(fx.client.get_run(&run)).await.unwrap();
        assert_eq!(summary.status, RunStatus::Interrupted);
        assert_eq!(summary.stop, Some(StopDetail::Cancelled { by: CancelledBy::Pause }));
        assert_eq!(summary.not_resumed, Some(NotResumed::Paused));
        assert_eq!(summary.session, Some(SessionRef("mock-session".into())));
        within(fx.client.resume(None)).await.unwrap();

        let attempt = resume_when_stopped(&fx, &run, Some("carry on".into())).await.unwrap();
        assert_eq!(attempt, 2);
        // The second attempt is live and held again; steering lets it finish.
        texts_until_held(&fx, &run, 2).await;
        let live = within(fx.client.get_run(&run)).await.unwrap();
        assert_eq!((live.attempt, live.not_resumed, live.stop), (2, None, None));
        within(fx.client.steer(&run, "go".into())).await.unwrap();
        let all = fx.events(&run, None).await;
        assert!(is_done(&all.last().unwrap().event, StopReason::Completed));
        assert!(
            all.iter().any(|i| matches!(&i.event, ExecEvent::Resumed { from_attempt: 1, session }
                if session.0 == "mock-session")),
            "the resume is in the journal"
        );
        let done = within(fx.client.get_run(&run)).await.unwrap();
        assert_eq!((done.status, done.attempt), (RunStatus::Succeeded, 2));
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_run_a_person_cancelled_is_cancelled_at_once_and_is_not_offered_for_resuming() {
        let fx = Fixture::start().await;
        let run = fx.run("mock-hold", "wait for me").await;
        texts_until_held(&fx, &run, 1).await;

        within(fx.client.cancel_run(&run, None)).await.unwrap();
        let first = fx.events(&run, None).await;
        let Some(ExecEvent::Done { reason, .. }) = first.last().map(|i| &i.event) else {
            panic!("no Done: {first:?}");
        };
        assert_eq!(*reason, Some(StopDetail::Cancelled { by: CancelledBy::User }));
        // Never `interrupted` on the way: this is the state clients read as stopped.
        let summary = within(fx.client.get_run(&run)).await.unwrap();
        assert_eq!(summary.status, RunStatus::Cancelled);
        assert_eq!(summary.stop, Some(StopDetail::Cancelled { by: CancelledBy::User }));
        assert_eq!(summary.not_resumed, None);
        let refused = resume_when_stopped(&fx, &run, None).await.unwrap_err();
        assert_eq!(
            refused_for(refused),
            NotResumed::WrongStatus { status: RunStatus::Cancelled }
        );
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_run_that_cannot_be_resumed_is_refused_with_a_typed_reason() {
        let fx = Fixture::start().await;
        let finished = fx.run("mock", "quick").await;
        fx.events(&finished, None).await;
        let refused = resume_when_stopped(&fx, &finished, None).await.unwrap_err();
        assert_eq!(
            refused_for(refused),
            NotResumed::WrongStatus { status: RunStatus::Succeeded }
        );

        let ghost = RunId::from_suffix("ghost").unwrap();
        let missing = within(fx.client.resume_run(&ghost, None)).await.unwrap_err();
        assert_eq!(missing.code(), Some(ErrorCode::NotFound));
        fx.stop().await;
    }
}
