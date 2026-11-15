#![cfg(test)]

//! Queues that must never wedge a run or a subscriber (ARCH-11).

use std::time::Duration;

use futures::StreamExt;
use reno_harness::exec::MockStep;
use reno_proto::{RunId, RunStatus};

use super::*;
use crate::KernelError;

#[tokio::test(start_paused = true)]
async fn a_flood_of_steers_is_refused_and_never_wedges_the_run() {
    let m = mock([MockStep::Sleep(Duration::from_secs(3600))]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let run = fx.submit("long").await;
    fx.wait_status(&run, RunStatus::Running).await;
    // The mock reads steering only when told to, so the queue fills.
    until(async || fx.kernel.steer(&run, "x").await.is_err().then_some(())).await;
    assert!(matches!(
        fx.kernel.steer(&run, "one more").await,
        Err(KernelError::SteerQueueFull(_))
    ));
    // The supervisor is still listening and still ends the run.
    fx.kernel.cancel(&run).await.unwrap();
    fx.wait_status(&run, RunStatus::Interrupted).await;
}

#[tokio::test(start_paused = true)]
async fn a_subscriber_to_a_queued_run_ends_when_the_run_is_cancelled_or_the_kernel_stops() {
    let m = mock([MockStep::Sleep(Duration::from_secs(3600))]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let first = fx.submit("first").await;
    let second = fx.submit("second").await;
    let third = fx.submit("third").await;
    fx.wait_status(&first, RunStatus::Running).await;

    let waiting = |run: &RunId| {
        let kernel = fx.kernel.clone();
        let run = run.clone();
        tokio::spawn(async move {
            kernel
                .run_events(&run, None)
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await
        })
    };
    let on_second = waiting(&second);
    let on_third = waiting(&third);
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(!on_second.is_finished(), "the run has not started");

    fx.kernel.cancel(&second).await.unwrap();
    until(async || on_second.is_finished().then_some(())).await;
    assert!(on_second.await.unwrap().is_empty());

    let kernel = fx.kernel.clone();
    let stopping = tokio::spawn(async move { kernel.shutdown().await });
    until(async || (stopping.is_finished() && on_third.is_finished()).then_some(())).await;
    assert!(on_third.await.unwrap().is_empty());
}
