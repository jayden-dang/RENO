//! `run_events`: replay a run's journal from a seq, then follow it live, with no gap and no
//! duplicate (ARCHITECTURE 17.2, ARCH-12).
//!
//! # The seam
//!
//! A live run's supervisor registers the subscriber while it holds the journal: the reply says
//! "the file holds every event below `next_seq`, flushed; everything from `next_seq` on will
//! arrive on your queue". The stream replays the file below `next_seq`, then drains the queue.
//! Replay is read in small batches on the blocking pool, so a long journal never blocks the
//! runtime or sits in memory.
//!
//! # Not live yet, or not live any more
//!
//! - A `queued` or `leased` run has no supervisor yet. The stream waits for the table of live
//!   runs to change (an event, not a poll) and tries again.
//! - Any other run without a supervisor has ended or been interrupted: its journal is complete,
//!   so the stream replays to the end of the file and finishes.
//!
//! # Falling behind
//!
//! The queue holds `KernelConfig::subscriber_queue` events. When it is full the supervisor drops
//! the subscriber and records the seq of the first event it did not queue; the stream then
//! delivers what it has and ends with `KernelError::Gap { resume_from }`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use reno_proto::{ExecEvent, RunEventItem, RunId, RunStatus, Seq};
use reno_store::{Journals, Replay};
use tokio::sync::{mpsc, oneshot, watch};

use crate::error::{KernelError, Result};
use crate::shared::{Shared, SupCmd};

/// Journal lines read per trip to the blocking pool.
const REPLAY_BATCH: usize = 64;

type Item = Result<RunEventItem>;

struct Tail {
    rx: mpsc::Receiver<(Seq, ExecEvent)>,
    overflow: Arc<AtomicU64>,
}

enum ReplayState {
    /// Not opened yet; the first batch opens it.
    Unopened,
    Open(Replay),
    /// Read to its end.
    Done,
}

enum Phase {
    /// Find out whether the run is live.
    Resolve,
    /// Read the journal. `until` is the live cut-off, or `None` to read to the end of the file.
    Replay {
        replay: ReplayState,
        until: Option<Seq>,
        tail: Option<Tail>,
        ready: VecDeque<Item>,
    },
    Tail(Tail),
    Finished,
}

struct Ctx {
    shared: Arc<Shared>,
    run: RunId,
    from: Seq,
    changed: watch::Receiver<u64>,
}

pub(crate) async fn run_events(
    shared: Arc<Shared>,
    run: RunId,
    from: Option<Seq>,
) -> Result<BoxStream<'static, Item>> {
    shared.run_record(&run).await?;
    let ctx = Ctx {
        changed: shared.live_changed.subscribe(),
        shared,
        run,
        // Journals count from 1 (`journal.rs`), so `Seq(0)` and "absent" both mean the start.
        from: from.unwrap_or(Seq(0)),
    };
    Ok(stream::unfold((ctx, Phase::Resolve), |(ctx, phase)| step(ctx, phase)).boxed())
}

/// Ask the run's supervisor to register a subscriber. `None`: the run is not live.
async fn subscribe(shared: &Shared, run: &RunId) -> Option<(Tail, Seq)> {
    let commands = shared.live_commands(run)?;
    let (reply, answer) = oneshot::channel();
    commands.send(SupCmd::Subscribe { reply }).await.ok()?;
    let subscription = answer.await.ok()?;
    let tail = Tail {
        rx: subscription.rx,
        overflow: subscription.overflow,
    };
    Some((tail, subscription.next_seq))
}

async fn step(mut ctx: Ctx, mut phase: Phase) -> Option<(Item, (Ctx, Phase))> {
    loop {
        phase = match phase {
            Phase::Finished => return None,
            Phase::Resolve => match resolve(&mut ctx).await {
                Ok(next) => next,
                Err(error) => return Some((Err(error), (ctx, Phase::Finished))),
            },
            Phase::Replay {
                replay,
                until,
                tail,
                mut ready,
            } => {
                if let Some(item) = ready.pop_front() {
                    let phase = Phase::Replay {
                        replay,
                        until,
                        tail,
                        ready,
                    };
                    return Some((item, (ctx, phase)));
                }
                let open = match replay {
                    ReplayState::Done => match tail {
                        Some(tail) => {
                            phase = Phase::Tail(tail);
                            continue;
                        }
                        None => return None,
                    },
                    ReplayState::Unopened => None,
                    ReplayState::Open(replay) => Some(replay),
                };
                let journals = ctx.shared.db.journals();
                match read_batch(journals, ctx.run.clone(), ctx.from, open, until).await {
                    Ok((replay, items, exhausted)) => Phase::Replay {
                        replay: if exhausted {
                            ReplayState::Done
                        } else {
                            ReplayState::Open(replay)
                        },
                        until,
                        tail,
                        ready: items,
                    },
                    Err(error) => return Some((Err(error), (ctx, Phase::Finished))),
                }
            }
            Phase::Tail(mut tail) => match tail.rx.recv().await {
                Some((seq, event)) if seq >= ctx.from => {
                    return Some((Ok(RunEventItem { seq, event }), (ctx, Phase::Tail(tail))));
                }
                Some(_) => Phase::Tail(tail),
                None => {
                    return match tail.overflow.load(Ordering::SeqCst) {
                        0 => None,
                        first_missed => Some((
                            Err(KernelError::Gap {
                                resume_from: Seq(first_missed),
                            }),
                            (ctx, Phase::Finished),
                        )),
                    };
                }
            },
        };
    }
}

/// Decide how to read the journal: live (replay below the cut-off, then the queue), waiting for
/// the run to go live, or to the end of the file.
async fn resolve(ctx: &mut Ctx) -> Result<Phase> {
    loop {
        // The status is read before the live table, and the order matters. A run enters the
        // table before it leaves `leased` and leaves it only after its end state and its `Done`
        // are written. So a status past `leased` followed by an absent table entry means the run
        // is over and its journal is complete; the other order could see "not live yet" and then
        // "already running" and wrongly conclude the same.
        let status = ctx.shared.run_record(&ctx.run).await?.status;
        if let Some((tail, until)) = subscribe(&ctx.shared, &ctx.run).await {
            return Ok(replay_phase(Some(until), Some(tail)));
        }
        if matches!(status, RunStatus::Queued | RunStatus::Leased) {
            // The run has no supervisor yet. Wait until some run enters or leaves the live
            // table, then look again. The receiver was created before the first look, so a
            // change in between is not lost.
            // A queued run that is cancelled never goes live, so `cancel` rings the same bell;
            // and a kernel that is shutting down will start nothing more.
            tokio::select! {
                changed = ctx.changed.changed() => {
                    if changed.is_err() {
                        return Ok(Phase::Finished);
                    }
                }
                () = ctx.shared.root.cancelled() => return Ok(Phase::Finished),
            }
            continue;
        }
        return Ok(replay_phase(None, None));
    }
}

fn replay_phase(until: Option<Seq>, tail: Option<Tail>) -> Phase {
    // The journal is opened by the first batch.
    Phase::Replay {
        replay: ReplayState::Unopened,
        until,
        tail,
        ready: VecDeque::new(),
    }
}

/// Read up to `REPLAY_BATCH` journal events from `from`, stopping before `until`. Returns the
/// reader, the events, and whether the journal is exhausted.
async fn read_batch(
    journals: Journals,
    run: RunId,
    from: Seq,
    replay: Option<Replay>,
    until: Option<Seq>,
) -> Result<(Replay, VecDeque<Item>, bool)> {
    let joined = tokio::task::spawn_blocking(move || {
        let mut replay = match replay {
            Some(replay) => replay,
            None => journals.replay(&run, from)?,
        };
        let mut items = VecDeque::new();
        let mut exhausted = false;
        while items.len() < REPLAY_BATCH {
            match replay.next() {
                Some(Ok((seq, event))) => {
                    if until.is_some_and(|until| seq >= until) {
                        exhausted = true;
                        break;
                    }
                    items.push_back(Ok(RunEventItem { seq, event }));
                }
                Some(Err(error)) => {
                    items.push_back(Err(KernelError::Store(error)));
                    exhausted = true;
                    break;
                }
                None => {
                    exhausted = true;
                    break;
                }
            }
        }
        Ok::<_, reno_store::StoreError>((replay, items, exhausted))
    })
    .await;
    match joined {
        Ok(result) => Ok(result?),
        Err(_) => Err(KernelError::Store(reno_store::StoreError::Closed)),
    }
}
