//! tokio -> GPUI bridge: a tokio task counts every 500 ms and the GPUI side applies each tick.

use std::time::Duration;

use gpui::{App, Entity};
use gpui_tokio::Tokio;
use tokio::sync::mpsc;

use crate::model::{Event, Model};

const PERIOD: Duration = Duration::from_millis(500);

/// Starts the ticker. The returned task lives inside the model; dropping the model cancels the
/// GPUI loop, and dropping the `Tokio::spawn` task inside it aborts the tokio task.
pub(crate) fn start_ticker(model: &Entity<Model>, cx: &mut App) {
    let (tx, mut rx) = mpsc::unbounded_channel::<u64>();

    let producer = Tokio::spawn(cx, async move {
        let mut interval = tokio::time::interval(PERIOD);
        let mut count = 0_u64;
        loop {
            interval.tick().await;
            count += 1;
            if tx.send(count).is_err() {
                break;
            }
        }
    });

    let weak = model.downgrade();
    let consumer = cx.spawn(async move |cx| {
        // Holding `producer` here ties the tokio task's life to this GPUI task.
        let _producer = producer;
        while let Some(count) = rx.recv().await {
            let applied = weak.update(cx, |model, cx| model.dispatch(Event::Tick(count), cx));
            if applied.is_err() {
                break;
            }
        }
    });

    model.update(cx, |model, _| model.hold_bridge(consumer));
}
