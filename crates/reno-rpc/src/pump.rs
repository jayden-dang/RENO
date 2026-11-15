//! The per-subscription pump: the bounded queue between a handler's stream and the socket
//! (ARCH-12).
//!
//! One task per subscription owns a `VecDeque` that never holds more than `capacity` items. Each
//! turn it either hands the front item to the connection writer (when the writer has room) or
//! pulls the next item from the handler's stream. Because the stream is only pulled between
//! sends, a slow socket shows up as a full queue and never as unbounded memory.
//!
//! Overflow policy, decided by the item that does not fit:
//! - it has a `Seq`: stop pulling, deliver what is queued, then send `sub.gap { resumeFrom }`
//!   where `resumeFrom` is that item's seq, the first one never delivered. The subscription ends.
//! - it has none (latest-value): the newest pending item is replaced by the new one, so the
//!   queue stays full and always ends with the most recent value.
//!
//! Dropping the stream (on cancel, disconnect or overflow) is how the source learns to stop.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use reno_proto::{Seq, SubscriptionId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::frame::{sub_done_line, sub_gap_line, sub_item_line};
use crate::router::{ErasedItem, ErasedStream};

/// Counters a test or an operator can read to check the bounds hold.
#[derive(Debug, Default)]
pub struct ServerStats {
    max_queue_len: AtomicUsize,
    gaps: AtomicUsize,
    coalesced: AtomicUsize,
}

impl ServerStats {
    /// The longest any subscription queue has ever been. Never exceeds the configured capacity.
    pub fn max_queue_len(&self) -> usize {
        self.max_queue_len.load(Ordering::Relaxed)
    }

    /// How many subscriptions were ended with `sub.gap`.
    pub fn gaps(&self) -> usize {
        self.gaps.load(Ordering::Relaxed)
    }

    /// How many latest-value items replaced a pending one.
    pub fn coalesced(&self) -> usize {
        self.coalesced.load(Ordering::Relaxed)
    }
}

enum End {
    Done,
    Gap(Seq),
}

pub(crate) struct Pump {
    pub(crate) sub: SubscriptionId,
    pub(crate) stream: ErasedStream,
    pub(crate) out: mpsc::Sender<String>,
    pub(crate) capacity: usize,
    pub(crate) token: CancellationToken,
    pub(crate) stats: Arc<ServerStats>,
}

impl Pump {
    pub(crate) async fn run(self) {
        let Self {
            sub,
            mut stream,
            out,
            capacity,
            token,
            stats,
        } = self;
        let capacity = capacity.max(1);
        let mut queue: VecDeque<ErasedItem> = VecDeque::new();
        let mut end: Option<End> = None;
        loop {
            if let (true, Some(end)) = (queue.is_empty(), &end) {
                let line = match end {
                    End::Done => sub_done_line(&sub),
                    End::Gap(seq) => sub_gap_line(&sub, *seq),
                };
                tokio::select! {
                    () = token.cancelled() => {}
                    _ = out.send(line) => {}
                }
                return;
            }
            let sending = !queue.is_empty();
            tokio::select! {
                biased;
                () = token.cancelled() => return,
                permit = out.reserve(), if sending => {
                    let Ok(permit) = permit else { return };
                    if let Some(item) = queue.pop_front() {
                        permit.send(sub_item_line(&sub, item.seq, item.value));
                    }
                }
                next = stream.next(), if end.is_none() => match next {
                    None => end = Some(End::Done),
                    Some(item) if queue.len() < capacity => {
                        queue.push_back(item);
                        stats.max_queue_len.fetch_max(queue.len(), Ordering::Relaxed);
                    }
                    Some(ErasedItem { seq: Some(seq), .. }) => {
                        stats.gaps.fetch_add(1, Ordering::Relaxed);
                        end = Some(End::Gap(seq));
                    }
                    Some(item) => {
                        if let Some(back) = queue.back_mut() {
                            *back = item;
                            stats.coalesced.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                },
            }
        }
    }
}
