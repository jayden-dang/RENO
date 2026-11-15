//! The run supervisor: one task per live run (ARCHITECTURE 17.2).
//!
//! It owns the executor's event stream, the run's `JournalWriter`, its subscribers, its pending
//! approvals and its heartbeat deadline, and it is the only writer of the run's state after the
//! dispatcher has started it. Everything that touches the run arrives as a message on its
//! bounded command queue, so no lock is shared with callers.
//!
//! # Events
//!
//! Every event the executor yields is appended to the journal, flushed, and then offered to each
//! subscriber, in that order and with no `.await` between the append and the offers, so a
//! subscriber that registers (a command, handled between events) sees a journal that ends
//! exactly where its queue begins. A subscriber whose queue is full is cut off with the seq of
//! the first event it missed (ARCH-12). The journal writes are blocking file I/O on this task:
//! each is one small buffered write and a flush, and the task is dedicated to the run.
//!
//! # Completion order (ARCH-8)
//!
//! On `Done` the run's terminal state commits to SQLite first (`lifecycle::end_run`), and only
//! then is `Done` appended to the journal and broadcast. If the commit fails, no `Done` is
//! written: the state is still non-terminal, and recovery interrupts the run and repairs the
//! journal from that state on the next start. Last, the slot is freed (by dropping it) and the
//! dispatcher wakes.
//!
//! # Waiting
//!
//! `NeedsApproval` moves the run to `waiting_approval` and adds a pending approval; `decide`
//! answers the executor and returns the run to `running`. `NeedsInput` moves it to
//! `waiting_input`. Phase 0 has no client path that answers input, so such a run waits until it
//! is cancelled, the executor gives up, or the kernel shuts down.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{ControlsClosed, Decision, ExecHandle, SteerRejected};
use reno_proto::{
    Approval, ApprovalId, ApprovalSubject, ExecEvent, RunId, RunStatus, Seq, StopReason,
};
use reno_store::{JournalWriter, StoreError};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::time::{Sleep, sleep};
use tokio_util::sync::CancellationToken;

use crate::error::{KernelError, Result};
use crate::lifecycle;
use crate::procs::{Records, Spawned};
use crate::shared::{Shared, Slot, Subscription, SupCmd};

/// A `run_events` subscriber's queue, from the supervisor's side.
#[derive(Debug)]
struct Sub {
    tx: mpsc::Sender<(Seq, ExecEvent)>,
    overflow: Arc<AtomicU64>,
}

impl Sub {
    /// Queue the event. Returns whether the subscriber stays registered.
    fn offer(&self, seq: Seq, event: &ExecEvent) -> bool {
        match self.tx.try_send((seq, event.clone())) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.overflow.store(seq.0, Ordering::SeqCst);
                false
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }
}

/// How the run ended, as the executor (or the supervisor, when it had to) said.
struct End {
    stop: StopReason,
    result: Option<String>,
    diagnostic: Option<String>,
}

pub(crate) struct Supervisor {
    shared: Arc<Shared>,
    run: RunId,
    fence: u64,
    stream: BoxStream<'static, ExecEvent>,
    handle: ExecHandle,
    cancel: CancellationToken,
    cmd_rx: mpsc::Receiver<SupCmd>,
    records: Records,
    status: RunStatus,
    journal: Option<JournalWriter>,
    subs: Vec<Sub>,
    open_approvals: Vec<(ApprovalId, String)>,
    open_inputs: HashSet<String>,
    /// Something went wrong that makes the run's record untrustworthy; the run ends `error`.
    fatal: Option<String>,
    /// The lease was taken over: this supervisor no longer owns the run and writes nothing.
    lost: bool,
    /// Last field: the slot is freed after everything else is dropped.
    _slot: Slot,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("run", &self.run)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Pending until `grace` is armed and elapses.
async fn grace_elapsed(grace: &mut Option<Pin<Box<Sleep>>>) {
    match grace {
        Some(sleep) => sleep.await,
        None => std::future::pending().await,
    }
}

impl Supervisor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        shared: Arc<Shared>,
        run: RunId,
        fence: u64,
        stream: BoxStream<'static, ExecEvent>,
        handle: ExecHandle,
        cancel: CancellationToken,
        cmd_rx: mpsc::Receiver<SupCmd>,
        slot: Slot,
    ) -> Self {
        let records = Records::take(&handle);
        Self {
            shared,
            run,
            fence,
            stream,
            handle,
            cancel,
            cmd_rx,
            records,
            status: RunStatus::Running,
            journal: None,
            subs: Vec::new(),
            open_approvals: Vec::new(),
            open_inputs: HashSet::new(),
            fatal: None,
            lost: false,
            _slot: slot,
        }
    }

    pub(crate) async fn run(mut self) {
        self.open_journal().await;
        let period = self.shared.config.heartbeat_period();
        let mut beat = Box::pin(sleep(period));
        let mut grace: Option<Pin<Box<Sleep>>> = None;
        let mut commands_open = true;
        let end = loop {
            tokio::select! {
                biased;
                spawned = self.records.next() => self.persist(spawned).await,
                cmd = self.cmd_rx.recv(), if commands_open => match cmd {
                    Some(cmd) => self.command(cmd).await,
                    None => commands_open = false,
                },
                () = &mut beat => {
                    self.heartbeat().await;
                    beat.set(sleep(period));
                }
                () = grace_elapsed(&mut grace) => {
                    // The executor ignored the cancel. Dropping its stream is what ends a
                    // harness that will not stop by itself.
                    self.stream = futures::stream::empty().boxed();
                    break End {
                        stop: StopReason::Interrupted,
                        result: None,
                        diagnostic: Some(format!(
                            "the executor did not stop within {:?} of the cancel",
                            self.shared.config.cancel_grace
                        )),
                    };
                }
                () = self.cancel.cancelled(), if grace.is_none() => {
                    grace = Some(Box::pin(sleep(self.shared.config.cancel_grace)));
                }
                event = self.stream.next() => match event {
                    Some(ExecEvent::Done { stop, result, diagnostic }) => {
                        break End { stop, result, diagnostic };
                    }
                    Some(event) => self.event(event).await,
                    None => {
                        break End {
                            stop: StopReason::Error,
                            result: None,
                            diagnostic: Some(
                                "the executor's stream ended without a Done".to_owned(),
                            ),
                        };
                    }
                },
            }
        };
        self.finish(end).await;
    }

    async fn open_journal(&mut self) {
        let journals = self.shared.db.journals();
        let run = self.run.clone();
        match tokio::task::spawn_blocking(move || journals.writer(&run)).await {
            Ok(Ok(writer)) => self.journal = Some(writer),
            Ok(Err(error)) => self.fail(format!("cannot open the run's journal: {error}")),
            Err(error) => self.fail(format!("journal task failed: {error}")),
        }
    }

    /// Give up on the run: it ends `error` with `why`.
    fn fail(&mut self, why: String) {
        tracing::error!(run = %self.run, %why, "ending the run");
        if self.fatal.is_none() {
            self.fatal = Some(why);
        }
        self.cancel.cancel();
    }

    /// Append to the journal, flush, and offer to subscribers. No `.await`: see the module doc.
    fn publish(&mut self, event: &ExecEvent) {
        let Some(journal) = self.journal.as_mut() else {
            return;
        };
        let appended = journal
            .append(event)
            .and_then(|seq| journal.flush().map(|()| seq));
        match appended {
            Ok(seq) => self.subs.retain(|sub| sub.offer(seq, event)),
            Err(error) => self.fail(format!("journal write failed: {error}")),
        }
    }

    fn next_seq(&self) -> Seq {
        self.journal
            .as_ref()
            .map_or(Seq(1), JournalWriter::next_seq)
    }

    /// Handle one non-final event. Every event is journaled and broadcast; the few that also
    /// change the run's state do that first, so whoever sees the event sees the state.
    async fn event(&mut self, event: ExecEvent) {
        if let ExecEvent::NeedsApproval {
            request_id,
            subject,
        } = &event
        {
            self.raise_approval(request_id, subject).await;
        } else if let ExecEvent::NeedsInput { request_id, .. } = &event {
            self.open_inputs.insert(request_id.clone());
            self.enter_wait(RunStatus::WaitingInput).await;
        }
        // Whatever the event is, it is recorded and broadcast.
        self.publish(&event);
    }

    async fn raise_approval(&mut self, request_id: &str, subject: &ApprovalSubject) {
        let requested = self.shared.db.now_ms();
        let suffix = self.shared.mint.suffix(requested);
        match ApprovalId::from_suffix(&suffix) {
            Ok(id) => {
                self.shared.approvals.add(Approval {
                    id: id.clone(),
                    run_id: self.run.clone(),
                    subject: subject.clone(),
                    requested_at_ms: u64::try_from(requested).unwrap_or(0),
                });
                self.open_approvals.push((id, request_id.to_owned()));
                self.enter_wait(RunStatus::WaitingApproval).await;
            }
            Err(error) => self.fail(format!("cannot mint an approval id: {error}")),
        }
    }

    async fn transition(&mut self, to: RunStatus) -> bool {
        match self.shared.db.transition(&self.run, self.fence, to).await {
            Ok(()) => {
                self.status = to;
                self.shared.mark_runs_dirty();
                true
            }
            Err(error) => {
                tracing::error!(run = %self.run, ?to, %error, "state transition failed");
                false
            }
        }
    }

    /// A run that is working goes to `waiting`; one that already waits stays where it is.
    async fn enter_wait(&mut self, waiting: RunStatus) {
        if self.status == RunStatus::Running {
            self.transition(waiting).await;
        }
    }

    /// Back to `running` once nothing is left to wait for.
    async fn resolve_wait(&mut self) {
        let waiting = matches!(
            self.status,
            RunStatus::WaitingApproval | RunStatus::WaitingInput
        );
        if waiting && self.open_approvals.is_empty() && self.open_inputs.is_empty() {
            self.transition(RunStatus::Running).await;
        }
    }

    async fn command(&mut self, cmd: SupCmd) {
        match cmd {
            SupCmd::Subscribe { reply } => {
                let (tx, rx) = mpsc::channel(self.shared.config.subscriber_queue);
                let overflow = Arc::new(AtomicU64::new(0));
                let subscription = Subscription {
                    next_seq: self.next_seq(),
                    rx,
                    overflow: Arc::clone(&overflow),
                };
                if reply.send(subscription).is_ok() {
                    self.subs.push(Sub { tx, overflow });
                }
            }
            SupCmd::Decide {
                approval,
                decision,
                reply,
            } => {
                let outcome = self.decide(approval, decision).await;
                // The caller may have gone away; the decision stands either way.
                let _ = reply.send(outcome);
            }
            SupCmd::Steer { prompt, reply } => {
                // Never wait here: the executor drains steering from inside the stream this task
                // reads, so a full queue would park the supervisor behind its own executor.
                let text = prompt.clone();
                let outcome = self
                    .handle
                    .try_steer(prompt)
                    .map_err(|rejected| match rejected {
                        SteerRejected::Full => KernelError::SteerQueueFull(self.run.clone()),
                        SteerRejected::Closed => KernelError::RunEnded(self.run.clone()),
                    });
                // An accepted steer is part of the transcript: journal it like any other event.
                if outcome.is_ok() {
                    self.publish(&ExecEvent::UserMessage { text });
                }
                let _ = reply.send(outcome);
            }
        }
    }

    async fn decide(&mut self, approval: ApprovalId, decision: Decision) -> Result<()> {
        let Some(at) = self
            .open_approvals
            .iter()
            .position(|(id, _)| *id == approval)
        else {
            return Err(KernelError::ApprovalNotFound(approval));
        };
        let request_id = self.open_approvals[at].1.clone();
        // A full answer queue must not park the supervisor either: give up if the run is
        // cancelled meanwhile.
        let sent = tokio::select! {
            sent = self.handle.decide(request_id, decision) => sent,
            () = self.cancel.cancelled() => Err(ControlsClosed),
        };
        sent.map_err(|_| KernelError::RunEnded(self.run.clone()))?;
        self.open_approvals.remove(at);
        self.shared.approvals.remove(&approval);
        self.resolve_wait().await;
        Ok(())
    }

    async fn heartbeat(&mut self) {
        let lease_ms = self.shared.config.lease_ms();
        match self
            .shared
            .db
            .heartbeat(&self.run, self.fence, lease_ms)
            .await
        {
            Ok(()) => {}
            Err(error @ (StoreError::StaleFence { .. } | StoreError::NotLeased { .. })) => {
                // Someone else owns the run now (ARCH-9). Stop the executor and write nothing.
                tracing::error!(run = %self.run, %error, "lost the lease");
                self.lost = true;
                self.cancel.cancel();
            }
            Err(error) => tracing::warn!(run = %self.run, %error, "heartbeat failed"),
        }
    }

    async fn persist(&mut self, spawned: Spawned) {
        let Spawned {
            pid,
            pgid,
            start_time,
        } = spawned;
        if let Err(error) = self
            .shared
            .db
            .record_process(&self.run, self.fence, pid, pgid, start_time)
            .await
        {
            tracing::warn!(run = %self.run, %error, "could not record the executor's process");
        }
    }

    async fn finish(&mut self, end: End) {
        // A process reported just before the end must reach the table before the run leaves it.
        while let Some(spawned) = self.records.try_next() {
            self.persist(spawned).await;
        }
        let End {
            mut stop,
            result,
            mut diagnostic,
        } = end;
        if let Some(why) = self.fatal.take() {
            stop = StopReason::Error;
            diagnostic = Some(why);
        }
        if self.lost {
            tracing::warn!(run = %self.run, "not ending a run whose lease was lost");
        } else {
            // State first, journal second (ARCH-8).
            match lifecycle::end_run(&self.shared.db, &self.run, self.fence, self.status, stop)
                .await
            {
                Ok(_) => self.publish(&ExecEvent::Done {
                    stop,
                    result,
                    diagnostic,
                }),
                Err(error) => tracing::error!(
                    run = %self.run, %error,
                    "could not commit the end of the run; recovery will interrupt it"
                ),
            }
        }
        self.subs.clear();
        self.shared.mark_runs_dirty();
    }
}
