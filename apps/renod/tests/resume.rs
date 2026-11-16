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
    async fn a_person_resumes_a_cancelled_run_and_it_continues_the_same_session() {
        let fx = Fixture::start().await;
        let run = fx.run("mock-hold", "wait for me").await;
        texts_until_held(&fx, &run, 1).await;

        within(fx.client.cancel_run(&run, None)).await.unwrap();
        // Wait for the journal to close: the run is `interrupted` and says who stopped it.
        let first = fx.events(&run, None).await;
        let Some(ExecEvent::Done { stop, reason, .. }) = first.last().map(|i| &i.event) else {
            panic!("no Done: {first:?}");
        };
        assert_eq!(*stop, StopReason::Interrupted);
        assert_eq!(*reason, Some(StopDetail::Cancelled { by: CancelledBy::User }));
        let summary = within(fx.client.get_run(&run)).await.unwrap();
        assert_eq!(summary.status, RunStatus::Interrupted);
        assert_eq!(summary.stop, Some(StopDetail::Cancelled { by: CancelledBy::User }));
        assert_eq!(summary.not_resumed, Some(NotResumed::Cancelled));
        assert_eq!(summary.session, Some(SessionRef("mock-session".into())));

        let attempt = within(fx.client.resume_run(&run, Some("carry on".into()))).await.unwrap();
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
    async fn a_run_that_cannot_be_resumed_is_refused_with_a_typed_reason() {
        let fx = Fixture::start().await;
        let finished = fx.run("mock", "quick").await;
        fx.events(&finished, None).await;
        let refused = within(fx.client.resume_run(&finished, None)).await.unwrap_err();
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
