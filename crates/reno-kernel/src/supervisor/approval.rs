//! What a run supervisor does with an approval request (ARCHITECTURE 8 point 4).
//!
//! # The flow
//!
//! On `NeedsApproval` the supervisor judges the subject, in this order:
//!
//! 1. The policy in force **now** is evaluated for this run's bot and worktree
//!    (`reno_proto::defs::evaluate`). A policy reload therefore changes the answer for requests
//!    that come after it and never for one already waiting.
//! 2. **Deny** by a rule: the harness is told no, with the rule named. Nobody is asked.
//! 3. A subject whose digest was already denied in this run is denied again, unasked
//!    (**repeat denial**). This overrides an allow rule and an ask, so a refusal stands for the
//!    rest of the run; a deny rule keeps its own name.
//! 4. **Allow** by a rule: the harness is told yes. Nobody is asked.
//! 5. **Ask**: a pending record is stored with a deadline, the run waits in
//!    `waiting_approval`, and the pending list is published. A human's `decide` answers it; if
//!    none comes by the deadline, it is denied as expired.
//!
//! Allow, deny and repeat-denial never change the run's status and never appear in the pending
//! list: the human does not see them. Every outcome is stored and journaled as
//! `ExecEvent::ApprovalResolved`, after the `NeedsApproval` it answers.
//!
//! # Order of effects
//!
//! A decision is committed (a `synchronous=FULL` write that also writes its event) **before** the
//! harness is told, and journaled after: an `Allow` that is not on record is never delivered
//! (ARCH-8, ARCH-9). If the record cannot be written the answer is **deny**, and the journal says
//! why.
//!
//! # The deadline
//!
//! One `Sleep` per supervisor, armed for the earliest deadline among its open approvals and
//! re-armed whenever the set changes. No timer exists while nothing waits, and nothing polls
//! (ARCH-10). Deadlines are `tokio::time` instants taken from a `Sleep`, so paused time drives
//! them in tests; the wall-clock `expires_at` that is stored comes from the `Clock` port.
//!
//! # Where an approval ends
//!
//! Besides a decision and a deadline, an open approval is closed `expired` when its run ends, and
//! by recovery (and by shutdown, for supervisors it dropped) when no supervisor is left to do
//! it. An expired approval's harness, if still there, was told no.

use std::time::Duration;

use reno_harness::exec::Decision;
use reno_proto::defs::{EvalContext, PolicyDecision, PolicyDef, RuleRef, evaluate};
use reno_proto::{ApprovalId, ApprovalOutcome, ApprovalSubject, ExecEvent, ResolvedBy, RunStatus};
use reno_store::{ExpireScope, NewApproval, StoreError};
use tokio::time::{Instant, sleep, sleep_until};

use super::Supervisor;
use crate::error::{KernelError, Result};

/// A request that waits for a human.
#[derive(Debug)]
pub(super) struct Open {
    id: ApprovalId,
    /// The harness's id, which the answer is sent against.
    request_id: String,
    /// When it expires, in `tokio::time`.
    deadline: Instant,
}

/// What the policy and the run's history say to do with a request.
#[derive(Debug)]
enum Verdict {
    /// Answer now.
    Auto {
        outcome: ApprovalOutcome,
        by: ResolvedBy,
        message: Option<String>,
    },
    /// Ask a human, who has `ttl` to answer.
    Ask { ttl: Duration },
}

fn named(rule: Option<RuleRef>) -> ResolvedBy {
    ResolvedBy::Rule {
        name: rule.map_or_else(
            || reno_proto::defs::DEFAULT_RULE.to_owned(),
            |rule| rule.name,
        ),
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn decision_of(outcome: ApprovalOutcome) -> Decision {
    match outcome {
        ApprovalOutcome::Approved => Decision::Allow,
        ApprovalOutcome::Denied => Decision::Deny,
    }
}

impl Supervisor {
    /// Handle a `NeedsApproval`: judge it, answer or hold it, and journal it (before its
    /// resolution, which a rule or a store failure may add at once).
    pub(super) async fn approval_requested(&mut self, event: ExecEvent) {
        let ExecEvent::NeedsApproval {
            request_id,
            subject,
        } = &event
        else {
            return;
        };
        match self.judge(subject).await {
            Ok(Verdict::Ask { ttl }) => {
                // State before visibility: the run reads `waiting_approval` before the pending
                // approval can be listed, and both before the journal shows the request.
                let raised = self.raise(request_id, subject, ttl).await;
                self.publish(&event);
                if let Err(why) = raised {
                    self.refuse_unrecorded(request_id, &why).await;
                }
            }
            Ok(Verdict::Auto {
                outcome,
                by,
                message,
            }) => {
                self.publish(&event);
                self.settle(request_id, subject, outcome, by, message).await;
            }
            Err(error) => {
                self.publish(&event);
                self.refuse_unrecorded(request_id, &error.to_string()).await;
            }
        }
    }

    async fn judge(
        &mut self,
        subject: &ApprovalSubject,
    ) -> std::result::Result<Verdict, StoreError> {
        let current = self.shared.current_defs();
        let fallback = PolicyDef::default();
        let policy = current.set().policy().map_or(&fallback, |p| p.def());
        let decision = evaluate(
            policy,
            &EvalContext {
                bot: self.info.bot.as_deref(),
                cwd: &self.info.cwd,
            },
            subject,
        );
        let asked = match decision {
            PolicyDecision::Deny { rule, message } => {
                return Ok(Verdict::Auto {
                    outcome: ApprovalOutcome::Denied,
                    by: named(rule),
                    message: Some(message),
                });
            }
            PolicyDecision::Allow { rule } => Verdict::Auto {
                outcome: ApprovalOutcome::Approved,
                by: named(rule),
                message: None,
            },
            PolicyDecision::Ask { .. } => Verdict::Ask {
                ttl: policy
                    .approval_ttl()
                    .unwrap_or(self.shared.config.approval_ttl),
            },
        };
        if self
            .shared
            .db
            .was_denied(&self.run, &subject.digest())
            .await?
        {
            return Ok(Verdict::Auto {
                outcome: ApprovalOutcome::Denied,
                by: ResolvedBy::RepeatDenial,
                message: None,
            });
        }
        Ok(asked)
    }

    fn mint(&self) -> std::result::Result<ApprovalId, String> {
        let suffix = self.shared.mint.suffix(self.shared.db.now_ms());
        ApprovalId::from_suffix(&suffix).map_err(|error| format!("cannot mint an id: {error}"))
    }

    fn new_record(
        &self,
        id: ApprovalId,
        request_id: &str,
        subject: &ApprovalSubject,
        expires_at: i64,
    ) -> NewApproval {
        NewApproval {
            id,
            run_id: self.run.clone(),
            request_id: request_id.to_owned(),
            subject: subject.clone(),
            expires_at,
        }
    }

    /// Store a pending request, move the run to `waiting_approval`, arm the deadline and publish
    /// the pending list. On failure nothing is left open.
    async fn raise(
        &mut self,
        request_id: &str,
        subject: &ApprovalSubject,
        ttl: Duration,
    ) -> std::result::Result<(), String> {
        let id = self.mint()?;
        // The deadline is read off a `Sleep` so that no clock is read here (ARCH-7).
        let deadline = sleep(ttl).deadline();
        let expires_at = self.shared.db.now_ms().saturating_add(millis(ttl));
        self.enter_wait(RunStatus::WaitingApproval).await;
        let new = self.new_record(id.clone(), request_id, subject, expires_at);
        self.shared
            .db
            .create_approval(new, self.fence)
            .await
            .map_err(|error| error.to_string())?;
        self.open_approvals.push(Open {
            id,
            request_id: request_id.to_owned(),
            deadline,
        });
        self.rearm();
        self.refresh_view().await;
        Ok(())
    }

    /// Record a decision that needs no human, tell the harness, and journal it.
    async fn settle(
        &mut self,
        request_id: &str,
        subject: &ApprovalSubject,
        outcome: ApprovalOutcome,
        by: ResolvedBy,
        message: Option<String>,
    ) {
        let recorded = match self.mint() {
            Ok(id) => {
                let new = self.new_record(id, request_id, subject, self.shared.db.now_ms());
                self.shared
                    .db
                    .record_approval(new, outcome, by.clone(), self.fence)
                    .await
                    .map_err(|error| error.to_string())
            }
            Err(why) => Err(why),
        };
        if let Err(why) = recorded {
            self.refuse_unrecorded(request_id, &why).await;
            return;
        }
        self.answer(request_id, outcome).await;
        self.publish(&ExecEvent::ApprovalResolved {
            request_id: request_id.to_owned(),
            decision: outcome,
            by,
            message,
        });
    }

    /// The decision could not be put on record, so it is not made: the harness is told no, and
    /// the journal says why. Failing closed is the only safe answer to a request nobody can
    /// audit.
    async fn refuse_unrecorded(&mut self, request_id: &str, why: &str) {
        tracing::error!(run = %self.run, request_id, %why, "approval not recorded; denying it");
        self.answer(request_id, ApprovalOutcome::Denied).await;
        self.publish(&ExecEvent::ApprovalResolved {
            request_id: request_id.to_owned(),
            decision: ApprovalOutcome::Denied,
            by: ResolvedBy::Expired,
            message: Some(format!("the approval could not be recorded: {why}")),
        });
        self.resolve_wait().await;
    }

    /// Tell the harness. Returns whether it was told: false when the run is being cancelled or
    /// its controls are closed. Never parks the supervisor behind a full answer queue.
    async fn answer(&mut self, request_id: &str, outcome: ApprovalOutcome) -> bool {
        let sent = tokio::select! {
            sent = self.handle.decide(request_id.to_owned(), decision_of(outcome)) => sent,
            () = self.cancel.cancelled() => Err(reno_harness::exec::ControlsClosed),
        };
        sent.is_ok()
    }

    /// A human's answer to one of this run's open approvals.
    pub(super) async fn decide(&mut self, approval: ApprovalId, decision: Decision) -> Result<()> {
        let Some(at) = self.open_approvals.iter().position(|o| o.id == approval) else {
            return Err(KernelError::ApprovalNotFound(approval));
        };
        let outcome = match decision {
            Decision::Allow => ApprovalOutcome::Approved,
            Decision::Deny => ApprovalOutcome::Denied,
        };
        // On record first; only then does the harness hear it.
        match self
            .shared
            .db
            .resolve_approval(&approval, outcome, ResolvedBy::Human, self.fence)
            .await
        {
            Ok(()) => {}
            Err(StoreError::NotFound { .. }) => {
                // Decided already by something else (recovery or shutdown); forget it here.
                self.open_approvals.remove(at);
                self.rearm();
                return Err(KernelError::ApprovalNotFound(approval));
            }
            Err(error) => return Err(error.into()),
        }
        let open = self.open_approvals.remove(at);
        self.rearm();
        let delivered = self.answer(&open.request_id, outcome).await;
        // State before visibility, as when raising: commit `running`, then drop it from the list.
        self.resolve_wait().await;
        self.refresh_view().await;
        self.publish(&ExecEvent::ApprovalResolved {
            request_id: open.request_id,
            decision: outcome,
            by: ResolvedBy::Human,
            message: None,
        });
        if delivered {
            Ok(())
        } else {
            Err(KernelError::RunEnded(self.run.clone()))
        }
    }

    /// The deadline fired: deny every open approval that is due.
    pub(super) async fn expire_due(&mut self) {
        let Some(fired) = self.expiry.as_ref().map(|timer| timer.deadline()) else {
            return;
        };
        let (due, rest): (Vec<Open>, Vec<Open>) = std::mem::take(&mut self.open_approvals)
            .into_iter()
            .partition(|open| open.deadline <= fired);
        self.open_approvals = rest;
        self.rearm();
        for open in due {
            if let Err(error) = self
                .shared
                .db
                .resolve_approval(
                    &open.id,
                    ApprovalOutcome::Denied,
                    ResolvedBy::Expired,
                    self.fence,
                )
                .await
            {
                tracing::error!(run = %self.run, approval = %open.id, %error, "could not record an expiry");
            }
            self.answer(&open.request_id, ApprovalOutcome::Denied).await;
            self.publish(&ExecEvent::ApprovalResolved {
                request_id: open.request_id,
                decision: ApprovalOutcome::Denied,
                by: ResolvedBy::Expired,
                message: None,
            });
        }
        self.resolve_wait().await;
        self.refresh_view().await;
    }

    /// The run is over: close what it still has open, in the store and in the journal.
    pub(super) async fn close_approvals(&mut self) {
        if self.open_approvals.is_empty() {
            return;
        }
        self.open_approvals.clear();
        self.expiry = None;
        let scope = ExpireScope::Run {
            run: self.run.clone(),
            fence: self.fence,
        };
        match self.shared.db.expire_pending(scope).await {
            Ok(expired) => {
                for closed in expired {
                    self.publish(&ExecEvent::ApprovalResolved {
                        request_id: closed.request_id,
                        decision: ApprovalOutcome::Denied,
                        by: ResolvedBy::Expired,
                        message: None,
                    });
                }
            }
            Err(error) => {
                tracing::error!(run = %self.run, %error, "could not close the run's approvals");
            }
        }
        self.refresh_view().await;
    }

    /// Point the one timer at the earliest open deadline, or drop it when nothing waits.
    fn rearm(&mut self) {
        let next = self.open_approvals.iter().map(|open| open.deadline).min();
        match (next, self.expiry.as_mut()) {
            (Some(deadline), Some(timer)) => timer.as_mut().reset(deadline),
            (Some(deadline), None) => self.expiry = Some(Box::pin(sleep_until(deadline))),
            (None, _) => self.expiry = None,
        }
    }

    /// Publish the pending list as the store now holds it.
    async fn refresh_view(&mut self) {
        if let Err(error) = self.shared.approvals.refresh(&self.shared.db).await {
            tracing::warn!(run = %self.run, %error, "could not refresh the pending approvals");
        }
    }
}
