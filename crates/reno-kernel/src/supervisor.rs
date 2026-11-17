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
//! `NeedsApproval` is judged by policy and standing rules (see the `approval` module, which has
//! the exact order): a rule or a repeated denial answers it at once and the run never leaves
//! `running` (a call to Reno's own tools is not journaled at all); otherwise the run moves to
//! `waiting_approval` with a stored pending approval, which `decide` or the approval's deadline
//! answers, and the run returns to `running`. `NeedsInput` moves it to `waiting_input`. No client
//! path answers input yet, so a question has the deadline an approval has (the policy's
//! `approval_ttl`, or the default, on the wall clock and re-checked on wake): when it passes
//! unanswered the harness is declined, the journal records `InputExpired` ("nobody answered in
//! time"), and the run returns to `running`. This applies whether or not a client is connected.

mod approval;

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{ExecHandle, SteerRejected};
use reno_proto::{
    BudgetKind, BudgetStop, CancelledBy, ExecEvent, RunId, RunReport, RunStatus, Seq, StopDetail,
    StopReason,
};
use reno_store::{JournalWriter, StoreError, Tokens};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::oneshot;
use tokio::time::{Sleep, sleep};
use tokio_util::sync::CancellationToken;

use crate::error::KernelError;
use crate::lifecycle;
use crate::procs::{Records, Spawned};
use crate::shared::{AgentDecision, Shared, Slot, StopCause, Subscription, SupCmd};
use crate::wake::{WakeSignal, system_woke};

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

/// What the supervisor must know about the run to judge its approvals.
#[derive(Debug, Clone)]
pub(crate) struct RunInfo {
    /// The executor the run stands on: where its unrecognised frames are counted (T1.14).
    pub(crate) executor: reno_proto::ExecutorId,
    /// The run's worktree, which policy paths are judged against.
    pub(crate) cwd: PathBuf,
    /// The bot the run is pinned to, which policy rules are scoped by.
    pub(crate) bot: Option<String>,
    /// The run's project (`Workspace::project_of`): resolved once, when the run starts, from the
    /// directory it was submitted for (not its worktree, which is inside Reno's data directory).
    /// Standing rules scoped to a project apply to a run only when this equals their root.
    pub(crate) project: Option<PathBuf>,
    /// The longest the run may take; the supervisor stops it when this has passed.
    pub(crate) wall_time: Duration,
    /// The daily token ceiling of the run's bot; the supervisor stops the run when its bot's
    /// tokens for the day reach it.
    pub(crate) token_ceiling: Option<u64>,
    /// The values of the run's secrets, when it has any: everything the executor yields passes
    /// through it before it is journaled, broadcast or stored.
    pub(crate) redactor: Option<Arc<reno_harness::ValueRedactor>>,
    /// Set when this attempt continues a session: the attempt that ended and the session. The
    /// supervisor journals it as a `Resumed` before anything else.
    pub(crate) resumed: Option<(u32, reno_proto::SessionRef)>,
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
    info: RunInfo,
    stream: BoxStream<'static, ExecEvent>,
    handle: ExecHandle,
    cancel: CancellationToken,
    /// Who cancelled the run, when someone other than the shutdown did.
    cause: StopCause,
    cmd_rx: mpsc::Receiver<SupCmd>,
    records: Records,
    status: RunStatus,
    journal: Option<JournalWriter>,
    subs: Vec<Sub>,
    /// Approvals waiting for a human, each with its deadline.
    open_approvals: Vec<approval::Open>,
    /// Fires at the earliest deadline in `open_approvals` and `open_inputs`; `None` while nothing waits.
    expiry: Option<Pin<Box<Sleep>>>,
    /// An expiry could not be recorded, so a row may still be pending that `open_approvals` no
    /// longer lists; the end of the run closes it.
    approvals_unrecorded: bool,
    /// Approvals the run asked for itself (`request_approval`), by the request id they carry in
    /// the journal, each with the one place its answer goes.
    agent_waiters: HashMap<String, oneshot::Sender<AgentDecision>>,
    /// Counts the request ids minted for `agent_waiters`.
    agent_seq: u64,
    /// Questions for a person that nobody has answered, each with its deadline (the same timer
    /// as `open_approvals`).
    open_inputs: Vec<approval::OpenInput>,
    /// Something went wrong that makes the run's record untrustworthy; the run ends `error`.
    fatal: Option<String>,
    /// The lease was taken over: this supervisor no longer owns the run and writes nothing.
    lost: bool,
    /// Fires when the run has used its wall time; `None` once it has, or once the run is
    /// stopping for another budget.
    wall: Option<Pin<Box<Sleep>>>,
    /// The budget that stopped the run, which becomes the diagnostic of its `Done` when the
    /// executor ends because of the cancel.
    budget: Option<BudgetStop>,
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

/// Pending until the earliest approval deadline is armed and elapses.
async fn expiry_elapsed(expiry: &mut Option<Pin<Box<Sleep>>>) {
    match expiry {
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
        info: RunInfo,
        stream: BoxStream<'static, ExecEvent>,
        handle: ExecHandle,
        cancel: CancellationToken,
        cause: StopCause,
        cmd_rx: mpsc::Receiver<SupCmd>,
        slot: Slot,
    ) -> Self {
        let records = Records::take(&handle);
        let stream = match info.redactor.clone() {
            Some(redactor) => stream
                .map(move |event| redactor.redact_event(event))
                .boxed(),
            None => stream,
        };
        Self {
            shared,
            run,
            fence,
            info,
            stream,
            handle,
            cancel,
            cause,
            cmd_rx,
            records,
            status: RunStatus::Running,
            journal: None,
            subs: Vec::new(),
            open_approvals: Vec::new(),
            expiry: None,
            approvals_unrecorded: false,
            agent_waiters: HashMap::new(),
            agent_seq: 0,
            open_inputs: Vec::new(),
            fatal: None,
            lost: false,
            wall: None,
            budget: None,
            _slot: slot,
        }
    }

    pub(crate) async fn run(mut self) {
        self.open_journal().await;
        if let Some((from_attempt, session)) = self.info.resumed.take() {
            self.publish(&ExecEvent::Resumed {
                from_attempt,
                session,
            });
        }
        self.wall = Some(Box::pin(sleep(self.info.wall_time)));
        #[cfg(test)]
        self.shared.db.armed(crate::db::Armed::WallTime);
        let period = self.shared.config.heartbeat_period();
        let mut beat = Box::pin(sleep(period));
        let mut grace: Option<Pin<Box<Sleep>>> = None;
        let mut commands_open = true;
        let mut woke = self.shared.system_wake.as_ref().map(WakeSignal::subscribe);
        let end = loop {
            tokio::select! {
                biased;
                () = system_woke(&mut woke) => self.expire_past_wall_clock().await,
                spawned = self.records.next() => self.persist(spawned).await,
                cmd = self.cmd_rx.recv(), if commands_open => match cmd {
                    Some(cmd) => self.command(cmd).await,
                    None => commands_open = false,
                },
                () = &mut beat => {
                    self.heartbeat().await;
                    beat.set(sleep(period));
                }
                () = expiry_elapsed(&mut self.expiry) => self.expire_due().await,
                () = grace_elapsed(&mut self.wall) => self.wall_time_up(),
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
                    Some(ExecEvent::Done { stop, result, diagnostic, .. }) => {
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
        if matches!(event, ExecEvent::NeedsApproval { .. }) {
            // Judged, answered or held, and journaled in its own order.
            self.approval_requested(event).await;
            return;
        }
        if let ExecEvent::NeedsInput { request_id, .. } = &event {
            self.input_requested(request_id).await;
        }
        if let ExecEvent::Started { session, .. } = &event {
            // The session is what a resume continues: it is durable before anyone sees it.
            if let Err(error) = self
                .shared
                .db
                .record_session(&self.run, self.fence, session)
                .await
            {
                tracing::warn!(run = %self.run, %error, "could not record the run's session");
            }
        }
        // Whatever the event is, it is recorded and broadcast.
        self.publish(&event);
        if let ExecEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
        } = event
        {
            self.charge(Tokens {
                input: input_tokens,
                output: output_tokens,
                cache_read: cache_read_tokens,
                cache_write: cache_write_tokens,
            })
            .await;
        }
    }

    /// Stops the run for `stop`: the executor is told to cancel, and the run ends `interrupted`
    /// with `stop` as its diagnostic. The first budget to stop a run is the one that is kept.
    fn stop_for_budget(&mut self, stop: BudgetStop) {
        if self.budget.is_some() {
            return;
        }
        tracing::warn!(run = %self.run, %stop, "stopping the run: a budget is spent");
        self.budget = Some(stop);
        self.wall = None;
        self.cancel.cancel();
    }

    /// The wall deadline passed.
    fn wall_time_up(&mut self) {
        self.wall = None;
        let ms = |span: Duration| u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        self.stop_for_budget(BudgetStop {
            kind: BudgetKind::WallTime,
            limit: ms(self.info.wall_time),
            // The deadline is the measure: the run has had exactly its limit.
            used: ms(self.info.wall_time),
        });
    }

    /// Adds a `Usage` event to the run's and its bot's tokens for the day, and stops the run when
    /// that takes the bot to its ceiling. A store failure is logged and the run goes on: the
    /// count is behind, which errs toward letting work finish.
    async fn charge(&mut self, add: Tokens) {
        if self.lost {
            return;
        }
        let bot = self.info.bot.clone();
        let total = self
            .shared
            .db
            .record_usage(&self.run, self.fence, bot.as_deref(), add)
            .await;
        match total {
            Ok(total) => {
                let counted = total.counted(self.shared.config.cache_read_permille());
                if let Some(limit) = self.info.token_ceiling
                    && counted >= limit
                {
                    self.stop_for_budget(BudgetStop {
                        kind: BudgetKind::Tokens,
                        limit,
                        used: counted,
                    });
                }
            }
            Err(error) => tracing::warn!(run = %self.run, %error, "could not record token usage"),
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
                message,
                remember,
                reply,
            } => {
                let outcome = self.decide(approval, decision, message, remember).await;
                // The caller may have gone away; the decision stands either way.
                let _ = reply.send(outcome);
            }
            SupCmd::Report { event, reply } => {
                self.publish(&event);
                // Beside the run's status, clients see what the agent said about its work.
                if let ExecEvent::AgentReport {
                    status, summary, ..
                } = &event
                {
                    let report = RunReport {
                        status: *status,
                        summary: summary.clone(),
                    };
                    if let Err(error) = self
                        .shared
                        .db
                        .set_report(&self.run, self.fence, &report)
                        .await
                    {
                        tracing::warn!(run = %self.run, %error, "could not record the run's report");
                    }
                }
                let _ = reply.send(Ok(()));
            }
            SupCmd::AgentApproval { subject, reply } => self.agent_approval(subject, reply).await,
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

    /// The typed reason an `interrupted` end has: the budget that stopped it, or whoever
    /// cancelled it, or the shutdown, or (when nobody asked) the executor stopping by itself.
    /// Any other end has none.
    fn stop_reason(&self, stop: StopReason) -> Option<StopDetail> {
        if stop != StopReason::Interrupted {
            return None;
        }
        Some(if let Some(budget) = self.budget {
            budget.into()
        } else if let Some(cause) = self.cause.get() {
            cause
        } else if self.shared.root.is_cancelled() {
            StopDetail::Shutdown
        } else if self.cancel.is_cancelled() {
            // Cancelled by a path that named no reason: treat it as a person's.
            StopDetail::Cancelled {
                by: CancelledBy::User,
            }
        } else {
            StopDetail::ExecutorStopped
        })
    }

    async fn finish(&mut self, end: End) {
        // The run is over, whatever the state says next: its token stops working now, and the
        // link's file goes (T1.9).
        crate::agent::revoke(&self.shared, &self.run).await;
        // A process reported just before the end must reach the table before the run leaves it.
        while let Some(spawned) = self.records.try_next() {
            self.persist(spawned).await;
        }
        let End {
            mut stop,
            result,
            mut diagnostic,
        } = end;
        if let Some(budget) = self.budget
            && stop == StopReason::Interrupted
        {
            // The cancel the budget asked for is what ended it; any other end (the harness
            // finished first, or failed) is reported as it came.
            diagnostic = Some(budget.to_diagnostic());
        }
        if let Some(why) = self.fatal.take() {
            stop = StopReason::Error;
            diagnostic = Some(why);
        }
        let reason = self.stop_reason(stop);
        self.shared.note_unknown_frames(&self.info.executor);
        if self.lost {
            tracing::warn!(run = %self.run, "not ending a run whose lease was lost");
        } else {
            // State first, journal second (ARCH-8).
            match lifecycle::end_run(
                &self.shared.db,
                &self.run,
                self.fence,
                self.status,
                stop,
                reason.as_ref(),
            )
            .await
            {
                Ok(_) => {
                    crate::workspaces::release_after_run(&self.shared, &self.run).await;
                    // What the run still had open is closed before its `Done`, which stays last.
                    self.close_approvals(reason.as_ref()).await;
                    self.publish(&ExecEvent::Done {
                        stop,
                        result,
                        diagnostic,
                        reason,
                    });
                }
                Err(error) => tracing::error!(
                    run = %self.run, %error,
                    "could not commit the end of the run; recovery will interrupt it"
                ),
            }
        }
        // A caller still waiting on an approval the run asked for hears the run ended.
        self.agent_waiters.clear();
        self.subs.clear();
        self.shared.mark_runs_dirty();
    }
}
