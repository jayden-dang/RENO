//! Plan usage on executor health: read on a refresh (and at start), throttled to once per 30 s
//! per executor, shown on the published health and never stored.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use reno_harness::exec::{MockExecutor, MockStep};
use reno_harness::usage::{UsageReading, UsageSource};
use reno_proto::{PlanUsage, UsageWindow};

use super::{Fx, START_MS, config, id};
use crate::ExecutorRegistry;

/// A usage source that answers what the test says and counts how often it was asked.
#[derive(Debug, Default)]
struct Scripted {
    says: Mutex<Option<UsageReading>>,
    reads: AtomicUsize,
}

impl Scripted {
    fn saying(session: f64) -> Arc<Self> {
        let source = Arc::new(Self::default());
        source.say(Some(reading(session)));
        source
    }

    fn say(&self, reading: Option<UsageReading>) {
        *self.says.lock().unwrap() = reading;
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl UsageSource for Scripted {
    async fn read(&self) -> Option<UsageReading> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.says.lock().unwrap().clone()
    }
}

fn reading(session: f64) -> UsageReading {
    UsageReading {
        plan_label: Some("ChatGPT Plus".into()),
        windows: vec![UsageWindow::new(
            "Session",
            session,
            Some(START_MS as u64 + 7_200_000),
        )],
    }
}

fn registry(source: Arc<Scripted>) -> ExecutorRegistry {
    ExecutorRegistry::new().with(Arc::new(
        MockExecutor::new(id("mock"))
            .with_steps([MockStep::text("hi")])
            .with_usage_source(source),
    ))
}

fn usage_of(fx: &Fx) -> Option<PlanUsage> {
    fx.kernel.executors()[0]
        .health
        .as_ref()
        .and_then(|h| h.plan_usage.clone())
}

#[tokio::test(start_paused = true)]
async fn a_refresh_reads_the_usage_stamps_it_and_publishes_it_with_the_health() {
    let source = Scripted::saying(0.54);
    let fx = Fx::new(config(1), registry(Arc::clone(&source))).await;
    let mut watch = fx.kernel.watch_executors();
    watch.borrow_and_update();

    let listed = fx.kernel.refresh_executors(None).await.unwrap();

    let usage = listed[0]
        .health
        .as_ref()
        .unwrap()
        .plan_usage
        .clone()
        .expect("usage");
    assert_eq!(usage.fetched_at_ms, START_MS as u64);
    assert_eq!(usage.plan_label.as_deref(), Some("ChatGPT Plus"));
    assert_eq!(usage.windows[0].used_permille, 540);
    assert!(watch.has_changed().unwrap());
    assert_eq!(
        watch.borrow_and_update()[0]
            .health
            .as_ref()
            .unwrap()
            .plan_usage,
        Some(usage)
    );
}

#[tokio::test(start_paused = true)]
async fn usage_is_read_at_most_once_in_30_seconds_whoever_asks() {
    let source = Scripted::saying(0.10);
    let fx = Fx::new(config(1), registry(Arc::clone(&source))).await;
    fx.kernel.refresh_executors(None).await.unwrap();
    fx.kernel.refresh_executors(None).await.unwrap();
    fx.kernel.refresh_executor_usage(None).await.unwrap();
    assert_eq!(source.reads(), 1);

    fx.clock.advance(29_000);
    fx.kernel.refresh_executor_usage(None).await.unwrap();
    assert_eq!(source.reads(), 1, "still inside the throttle");

    source.say(Some(reading(0.20)));
    fx.clock.advance(1_000);
    fx.kernel.refresh_executor_usage(None).await.unwrap();
    assert_eq!(source.reads(), 2);
    let usage = usage_of(&fx).expect("usage");
    assert_eq!(usage.windows[0].used_permille, 200);
    assert_eq!(usage.fetched_at_ms, START_MS as u64 + 30_000);
}

#[tokio::test(start_paused = true)]
async fn a_usage_only_refresh_of_an_unprobed_executor_probes_it_first() {
    let source = Scripted::saying(0.30);
    let fx = Fx::new(config(1), registry(Arc::clone(&source))).await;
    assert!(fx.kernel.executors()[0].health.is_none());
    let listed = fx
        .kernel
        .refresh_executor_usage(Some(&id("mock")))
        .await
        .unwrap();
    let health = listed[0].health.as_ref().expect("probed");
    assert!(health.plan_usage.is_some());
}

#[tokio::test(start_paused = true)]
async fn an_unavailable_reading_keeps_the_last_good_one_for_half_an_hour_then_drops_it() {
    let source = Scripted::saying(0.54);
    let fx = Fx::new(config(1), registry(Arc::clone(&source))).await;
    fx.kernel.refresh_executors(None).await.unwrap();
    source.say(None);

    fx.clock.advance(60_000);
    fx.kernel.refresh_executor_usage(None).await.unwrap();
    let kept = usage_of(&fx).expect("kept");
    assert_eq!(
        kept.fetched_at_ms, START_MS as u64,
        "its own age is what it shows"
    );

    fx.clock.advance(30 * 60_000);
    fx.kernel.refresh_executor_usage(None).await.unwrap();
    assert_eq!(usage_of(&fx), None);
}

#[tokio::test(start_paused = true)]
async fn usage_is_not_stored_with_the_health() {
    let source = Scripted::saying(0.54);
    let fx = Fx::new(config(1), registry(source)).await;
    fx.kernel.refresh_executors(None).await.unwrap();
    assert!(usage_of(&fx).is_some());
    let stored = fx.store.executor_health().await.unwrap();
    assert_eq!(stored[0].1.plan_usage, None);
}

#[tokio::test(start_paused = true)]
async fn an_executor_without_a_source_has_no_usage() {
    let bare = ExecutorRegistry::new().with(Arc::new(MockExecutor::new(id("mock"))));
    let fx = Fx::new(config(1), bare).await;
    fx.kernel.refresh_executors(None).await.unwrap();
    assert_eq!(usage_of(&fx), None);
}

#[tokio::test(start_paused = true)]
async fn an_unknown_executor_is_not_found() {
    let fx = Fx::new(config(1), registry(Scripted::saying(0.1))).await;
    let error = fx
        .kernel
        .refresh_executor_usage(Some(&id("nowhere")))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::KernelError::ExecutorNotFound(_)),
        "{error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_signed_out_executor_is_not_asked_for_usage() {
    let source = Scripted::saying(0.54);
    let probed = super::health::Probed::new(super::health::signed_out())
        .with_usage(Arc::clone(&source) as Arc<dyn UsageSource>);
    let fx = Fx::new(config(1), probed.registry()).await;
    fx.kernel.refresh_executors(None).await.unwrap();
    assert_eq!(source.reads(), 0);
    assert_eq!(usage_of(&fx), None);
}
