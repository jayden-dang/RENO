//! What a run supervisor does with an approval request (ARCHITECTURE 8 point 4).
//!
//! # The flow
//!
//! On `NeedsApproval` the supervisor judges the subject, in this order. The first step that
//! applies answers; nothing after it is consulted.
//!
//! 1. **Policy deny.** The policy in force **now** (`reno_proto::defs::evaluate`, for this run's
//!    bot and worktree) denies it, by a rule of `policy.toml` or by the policy's default. The
//!    harness is told no, with the rule named. An explicit deny in the file therefore beats
//!    everything below, standing rules included, and so does a default of `deny` (a person is
//!    never asked, so there is nothing for a standing rule to answer). A policy reload changes the
//!    answer for requests that come after it and never for one already waiting.
//! 2. **Standing deny.** A standing rule of effect `deny` that is in force and matches the
//!    subject (`reno_proto::rules`; scope: this run, this run's bot, this run's project, or
//!    everything). A standing deny beats a standing allow whatever their scopes, and beats a
//!    policy `allow` or `ask`.
//!    The decision is named `standing:<rule id>`.
//! 3. **Repeat denial.** A subject whose digest was already refused in this run, by a person, by
//!    a rule or by running out of time (no answer means deny), is denied again, unasked. This
//!    overrides an allow rule, a standing allow and an ask, so a refusal stands for the rest of
//!    the run, and a harness that retries an unanswered action is not held for another full time
//!    limit; a deny keeps its own name.
//! 4. **Policy allow.** A rule of the policy allows it (or the policy's default does).
//! 5. **Standing allow.** A standing rule of effect `allow` that is in force and matches.
//! 6. **Reno's own tools.** Under `ask`, a call to a tool of Reno's own MCP server is allowed as
//!    `builtin:reno-mcp`. This one is **not journaled** as a `NeedsApproval`/`ApprovalResolved`
//!    pair: it is recorded in the approval history (the `approvals` row and its
//!    `approval.decided` event, which name the exact tool and the digest), and the journal stays
//!    free of a pair per `report` or board call. If the record cannot be written the request is
//!    journaled first and then refused, like any other.
//! 7. **Full access.** A run launched with `AccessMode::FullAccess` is never asked: what steps 1 to
//!    3 did not refuse is allowed at once, decided by `builtin:full-access`, stored and journaled
//!    like any other automatic answer. So a policy deny, a standing deny and a repeat denial hold
//!    in every access mode, and the audit trail has the same rows in all of them.
//! 8. **Ask.** A pending record is stored with a deadline, the run waits in `waiting_approval`,
//!    and the pending list is published. A human's `decide` answers it; if none comes by the
//!    deadline, it is denied as expired.
//!
//! In the owner's words the order is **never** (step 1, the policy file) → **this project** → **every
//! project** → **ask**: a project rule is judged with the others of its effect (steps 2 and 5), so it
//! can never loosen a policy deny (step 1 answers first), a project deny refuses what a global allow
//! would have allowed, and a project allow answers what would otherwise be asked. A run's project is
//! the git repository root of the directory it was submitted for (`Workspace::project_of`), resolved once
//! when the run starts; a run with no project meets no project rule.
//!
//! A standing rule is in force when it is not revoked and `now < expires_at`; expiry is read
//! against the clock at the time of judging, with no timer. A rule that is revoked while a
//! request is being judged may still answer that request.
//!
//! Allow, deny and repeat-denial never change the run's status and never appear in the pending
//! list: the human does not see them. Every outcome except step 6 is stored and journaled as
//! `ExecEvent::ApprovalResolved`, after the `NeedsApproval` it answers.
//!
//! # Remembering a decision
//!
//! `decide` may carry a [`Remember`]. The rule is made from the open approval's own subject
//! (`RuleMatcher::derive`, never broader than it) and is committed in the same transaction as the
//! decision, so a rule exists only for a decision that was made. Its effect is the decision's.
//! A person's approval is `approve once` for the request and `allow` for the rule. An approval the
//! run asked for itself through `request_approval` cannot be remembered (`Invalid`): its tool is
//! constant and what distinguishes it is free text the run wrote.
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
//! An approval's limit is wall-clock time (`expires_at`), but the timer is monotonic and stops
//! while a Mac sleeps. When the system wakes ([`WakeSignal`](crate::WakeSignal)) the supervisor
//! reads the clock and expires, at once and through the same path as the timer (record, answer,
//! journal), every approval whose `expires_at` has passed.
//!
//! A question for a person (`NeedsInput`) gets the same limit and the same two clocks: the
//! policy's `approval_ttl`, or the default. When it passes with nobody having answered, the
//! harness is declined (an empty answer list, which every driver reads as "no answer"), the
//! journal gets `ExecEvent::InputExpired`, and the run returns to `running`. This holds whether
//! or not a client is connected: no client answers questions yet.
//!
//! # Where an approval ends
//!
//! Besides a decision and a deadline, an open approval is closed `expired` when its run ends, and
//! by recovery (and by shutdown, for supervisors it dropped) when no supervisor is left to do
//! it. An expired approval's harness, if still there, was told no.

use std::time::Duration;

use reno_harness::exec::Decision;
use reno_proto::defs::{EvalContext, PolicyDecision, PolicyDef, RuleRef, evaluate};
use reno_proto::{
    AccessMode, ActionClass, ApprovalId, ApprovalOutcome, ApprovalSubject, ExecEvent,
    MCP_SERVER_NAME, MCP_TOOLS, Remember, RememberScope, ResolvedBy, RuleId, RuleMatcher,
    RunStatus, StandingEffect, StandingRule, StandingScope, StopDetail, standing_rule_name,
};
use reno_store::{ExpireScope, NewApproval, NewRule, StoreError};
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep, sleep_until};

use super::Supervisor;
use crate::error::{KernelError, Result};
use crate::shared::AgentDecision;

/// A request that waits for a human.
#[derive(Debug)]
pub(super) struct Open {
    id: ApprovalId,
    /// What was asked, which a remembered answer is derived from.
    subject: ApprovalSubject,
    /// The harness's id, which the answer is sent against.
    request_id: String,
    /// When it expires, in `tokio::time`: the timer that normally closes it.
    deadline: Instant,
    /// When it expires on the wall clock, as stored. The timer stops while the machine sleeps,
    /// so a wake-up closes the approval by this one.
    expires_at: i64,
}

/// A question for a person (`NeedsInput`) that nobody has answered yet. It expires after the
/// same limit as an approval, with the same two clocks.
#[derive(Debug)]
pub(super) struct OpenInput {
    request_id: String,
    /// When it expires, in `tokio::time`.
    deadline: Instant,
    /// When it expires on the wall clock; a wake-up closes it by this one.
    expires_at: i64,
}

/// What the policy and the run's history say to do with a request.
#[derive(Debug)]
enum Verdict {
    /// Answer now.
    Auto {
        outcome: ApprovalOutcome,
        by: ResolvedBy,
        message: Option<String>,
        /// Whether the journal shows the request and its answer. False only for Reno's own
        /// tools (`builtin:reno-mcp`), which the approval history alone records.
        journal: bool,
    },
    /// Ask a human, who has `ttl` to answer.
    Ask { ttl: Duration },
}

/// The name the built-in allowance of Reno's own MCP tools goes by in the record.
const RENO_TOOL_RULE: &str = "builtin:reno-mcp";

/// The name a `FullAccess` run's unrefused requests are allowed under in the record.
const FULL_ACCESS_RULE: &str = "builtin:full-access";

/// The most approvals one run may have asked for through `request_approval` at once.
const MAX_AGENT_APPROVALS: usize = 8;

/// Whether `subject` is a call to one of the tools of Reno's own MCP server, by the name a
/// harness gives it (`mcp__reno__report`). Exact names only: another server's tool, or a tool of
/// Reno's that does not exist, is not covered.
fn is_reno_tool(subject: &ApprovalSubject) -> bool {
    subject.action == ActionClass::McpTool
        && MCP_TOOLS
            .iter()
            .any(|tool| subject.tool == format!("mcp__{MCP_SERVER_NAME}__{tool}"))
}

fn named(rule: Option<RuleRef>) -> ResolvedBy {
    ResolvedBy::Rule {
        name: rule.map_or_else(
            || reno_proto::defs::DEFAULT_RULE.to_owned(),
            |rule| rule.name,
        ),
    }
}

fn standing_by(rule: &StandingRule) -> ResolvedBy {
    ResolvedBy::Rule {
        name: standing_rule_name(&rule.id),
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
    /// An approval the run asked for through Reno's MCP server: raised like a harness's own,
    /// and answered to the caller waiting on `reply` instead of the harness.
    pub(super) async fn agent_approval(
        &mut self,
        subject: ApprovalSubject,
        reply: oneshot::Sender<AgentDecision>,
    ) {
        if self.agent_waiters.len() >= MAX_AGENT_APPROVALS {
            let decision = AgentDecision {
                outcome: ApprovalOutcome::Denied,
                by: ResolvedBy::Rule {
                    name: "builtin:too-many-requests".to_owned(),
                },
                message: Some("too many approval requests are waiting; wait for one".to_owned()),
            };
            let _ = reply.send(decision);
            return;
        }
        self.agent_seq += 1;
        let request_id = format!("agent-{}", self.agent_seq);
        self.agent_waiters.insert(request_id.clone(), reply);
        self.approval_requested(ExecEvent::NeedsApproval {
            request_id,
            subject,
        })
        .await;
    }

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
                journal,
            }) => {
                if journal {
                    self.publish(&event);
                }
                let request = (!journal).then_some(&event);
                self.settle(request_id, subject, (outcome, by, message), request)
                    .await;
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
        let auto = |outcome, by, message| Verdict::Auto {
            outcome,
            by,
            message,
            journal: true,
        };
        // 1. The policy's own denials come first and are final.
        if let PolicyDecision::Deny { rule, message } = decision {
            return Ok(auto(ApprovalOutcome::Denied, named(rule), Some(message)));
        }
        // 2. Standing denials.
        let standing = self
            .shared
            .db
            .active_rules(
                &self.run,
                self.info.bot.as_deref(),
                self.info.project.as_deref(),
            )
            .await?;
        let of_effect = |effect: StandingEffect| {
            standing.iter().find(|rule| {
                rule.effect == effect && rule.matcher.matches(effect, &self.info.cwd, subject)
            })
        };
        if let Some(rule) = of_effect(StandingEffect::Deny) {
            return Ok(auto(
                ApprovalOutcome::Denied,
                standing_by(rule),
                Some(format!("denied by a standing rule ({})", rule.id)),
            ));
        }
        // 3. A refusal in this run stands.
        if self
            .shared
            .db
            .was_denied(&self.run, &subject.digest())
            .await?
        {
            return Ok(auto(
                ApprovalOutcome::Denied,
                ResolvedBy::RepeatDenial,
                None,
            ));
        }
        // 4. The policy allows it.
        if let PolicyDecision::Allow { rule } = decision {
            return Ok(auto(ApprovalOutcome::Approved, named(rule), None));
        }
        // 5. A standing allow.
        if let Some(rule) = of_effect(StandingEffect::Allow) {
            return Ok(auto(ApprovalOutcome::Approved, standing_by(rule), None));
        }
        // 6. Reno's own tools need no second question: where one is an approval, it raises its
        // own, and the rest touch only the run's own board and journal (ARCHITECTURE 8). A rule
        // that denies them still wins, above.
        if is_reno_tool(subject) {
            return Ok(Verdict::Auto {
                outcome: ApprovalOutcome::Approved,
                by: ResolvedBy::Rule {
                    name: RENO_TOOL_RULE.to_owned(),
                },
                message: None,
                journal: false,
            });
        }
        // 7. Full access asks nobody: what steps 1-3 did not refuse is allowed, on record as
        // decided by the access mode.
        if self.info.access == AccessMode::FullAccess {
            return Ok(auto(
                ApprovalOutcome::Approved,
                ResolvedBy::Rule {
                    name: FULL_ACCESS_RULE.to_owned(),
                },
                None,
            ));
        }
        // 8. Ask.
        Ok(Verdict::Ask {
            ttl: policy
                .approval_ttl()
                .unwrap_or(self.shared.config.approval_ttl),
        })
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

    /// The time limit of an approval or a question: the policy's `approval_ttl`, or the default.
    fn approval_ttl(&self) -> Duration {
        let current = self.shared.current_defs();
        current
            .set()
            .policy()
            .and_then(|p| p.def().approval_ttl())
            .unwrap_or(self.shared.config.approval_ttl)
    }

    /// A question for a person was asked: the run waits in `waiting_input` and the question gets
    /// the deadline an approval would.
    pub(super) async fn input_requested(&mut self, request_id: &str) {
        let ttl = self.approval_ttl();
        self.open_inputs.push(OpenInput {
            request_id: request_id.to_owned(),
            deadline: sleep(ttl).deadline(),
            expires_at: self.shared.db.now_ms().saturating_add(millis(ttl)),
        });
        self.rearm();
        self.enter_wait(RunStatus::WaitingInput).await;
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
            subject: subject.clone(),
            request_id: request_id.to_owned(),
            deadline,
            expires_at,
        });
        self.rearm();
        self.refresh_view().await;
        Ok(())
    }

    /// Record a decision that needs no human, tell the harness, and journal it.
    ///
    /// With `quiet` (the request) neither the request nor its answer is journaled: the approval
    /// history holds them. The one exception is a decision that cannot be recorded: the request
    /// is then journaled first, so the refusal that follows has its request.
    async fn settle(
        &mut self,
        request_id: &str,
        subject: &ApprovalSubject,
        (outcome, by, message): (ApprovalOutcome, ResolvedBy, Option<String>),
        quiet: Option<&ExecEvent>,
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
            if let Some(request) = quiet {
                self.publish(request);
            }
            self.refuse_unrecorded(request_id, &why).await;
            return;
        }
        // A refusal carries the rule's message to the harness as well as to the journal.
        let for_harness = message
            .clone()
            .filter(|_| outcome == ApprovalOutcome::Denied);
        self.answer(request_id, outcome, for_harness, &by).await;
        if quiet.is_none() {
            self.publish(&ExecEvent::ApprovalResolved {
                request_id: request_id.to_owned(),
                decision: outcome,
                by,
                message,
            });
        }
    }

    /// The decision could not be put on record, so it is not made: the harness is told no, and
    /// the journal says why. Failing closed is the only safe answer to a request nobody can
    /// audit.
    async fn refuse_unrecorded(&mut self, request_id: &str, why: &str) {
        tracing::error!(run = %self.run, request_id, %why, "approval not recorded; denying it");
        self.answer(
            request_id,
            ApprovalOutcome::Denied,
            None,
            &ResolvedBy::Expired,
        )
        .await;
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
    async fn answer(
        &mut self,
        request_id: &str,
        outcome: ApprovalOutcome,
        message: Option<String>,
        by: &ResolvedBy,
    ) -> bool {
        // An approval the run asked for itself is answered to the caller waiting on it, never to
        // the harness.
        if let Some(waiter) = self.agent_waiters.remove(request_id) {
            let decision = AgentDecision {
                outcome,
                by: by.clone(),
                message,
            };
            return waiter.send(decision).is_ok();
        }
        let sent = tokio::select! {
            sent = self.handle.decide_with_message(request_id.to_owned(), decision_of(outcome), message) => sent,
            () = self.cancel.cancelled() => Err(reno_harness::exec::ControlsClosed),
        };
        sent.is_ok()
    }

    /// A human's answer to one of this run's open approvals.
    ///
    /// A denial's `message` is recorded with the decision, reaches the harness as a rule's
    /// message does, and is journalled; an approval has none.
    pub(super) async fn decide(
        &mut self,
        approval: ApprovalId,
        decision: Decision,
        message: Option<String>,
        remember: Option<Remember>,
    ) -> Result<()> {
        let Some(at) = self.open_approvals.iter().position(|o| o.id == approval) else {
            return Err(KernelError::ApprovalNotFound(approval));
        };
        let outcome = match decision {
            Decision::Allow => ApprovalOutcome::Approved,
            Decision::Deny => ApprovalOutcome::Denied,
        };
        let message = message.filter(|_| outcome == ApprovalOutcome::Denied);
        let rule = match &remember {
            Some(_)
                if self
                    .agent_waiters
                    .contains_key(&self.open_approvals[at].request_id) =>
            {
                // The run wrote this subject itself: its tool is always `request_approval` and
                // the part that says why is free text, so a rule derived from it would cover
                // everything the run might ask for next.
                return Err(KernelError::Invalid(
                    "an approval the run asked for itself cannot be kept as a rule; answer it once"
                        .to_owned(),
                ));
            }
            Some(remember) => Some(self.rule_from(&self.open_approvals[at], outcome, remember)?),
            None => None,
        };
        // On record first; only then does the harness hear it.
        let recorded = match rule {
            Some(rule) => {
                self.shared
                    .db
                    .resolve_approval_remembering(
                        &approval,
                        outcome,
                        ResolvedBy::Human,
                        message.clone(),
                        rule,
                        self.fence,
                    )
                    .await
            }
            None => {
                self.shared
                    .db
                    .resolve_approval(
                        &approval,
                        outcome,
                        ResolvedBy::Human,
                        message.clone(),
                        self.fence,
                    )
                    .await
            }
        };
        match recorded {
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
        let delivered = self
            .answer(
                &open.request_id,
                outcome,
                message.clone(),
                &ResolvedBy::Human,
            )
            .await;
        // State before visibility, as when raising: commit `running`, then drop it from the list.
        self.resolve_wait().await;
        self.refresh_view().await;
        self.publish(&ExecEvent::ApprovalResolved {
            request_id: open.request_id,
            decision: outcome,
            by: ResolvedBy::Human,
            message,
        });
        if delivered {
            Ok(())
        } else {
            Err(KernelError::RunEnded(self.run.clone()))
        }
    }

    /// The standing rule that remembering `outcome` for `open` makes: effect of the answer,
    /// matcher derived from the subject, scope and expiry as asked.
    fn rule_from(
        &self,
        open: &Open,
        outcome: ApprovalOutcome,
        remember: &Remember,
    ) -> Result<NewRule> {
        let scope = match remember.scope {
            RememberScope::Run => StandingScope::Run {
                run: self.run.clone(),
            },
            RememberScope::Bot => StandingScope::Bot {
                bot: self.info.bot.clone().ok_or_else(|| {
                    KernelError::Invalid(
                        "this run has no bot, so a rule cannot be kept for its bot".to_owned(),
                    )
                })?,
            },
            RememberScope::Project => StandingScope::Project {
                root: self.info.project.clone().ok_or_else(|| {
                    KernelError::Invalid(
                        "this run is not in a git repository, so it has no project to keep a rule for"
                            .to_owned(),
                    )
                })?,
            },
            RememberScope::Always => StandingScope::Always,
        };
        let suffix = self.shared.mint.suffix(self.shared.db.now_ms());
        let id = RuleId::from_suffix(&suffix)
            .map_err(|error| KernelError::Invalid(format!("cannot mint a rule id: {error}")))?;
        let expires_at = remember.expires_in_secs.map(|secs| {
            self.shared
                .db
                .now_ms()
                .saturating_add(i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX))
        });
        Ok(NewRule {
            id,
            scope,
            effect: match outcome {
                ApprovalOutcome::Approved => StandingEffect::Allow,
                ApprovalOutcome::Denied => StandingEffect::Deny,
            },
            matcher: RuleMatcher::derive(&open.subject, &self.info.cwd),
            expires_at,
            source_approval: None,
        })
    }

    /// The deadline fired: deny every open approval that is due.
    pub(super) async fn expire_due(&mut self) {
        let Some(fired) = self.expiry.as_ref().map(|timer| timer.deadline()) else {
            return;
        };
        self.expire_where(
            |open| open.deadline <= fired,
            |input| input.deadline <= fired,
        )
        .await;
    }

    /// The system woke, and the timer did not run while it slept: deny every open approval whose
    /// wall-clock `expires_at` has passed on the clock now, exactly as the timer would have.
    pub(super) async fn expire_past_wall_clock(&mut self) {
        let now = self.shared.db.now_ms();
        self.expire_where(
            |open| open.expires_at <= now,
            |input| input.expires_at <= now,
        )
        .await;
    }

    /// Deny, as expired, every open approval `is_due` picks, decline every open question
    /// `input_due` picks, and re-arm the timer for the rest.
    async fn expire_where(
        &mut self,
        is_due: impl Fn(&Open) -> bool,
        input_due: impl Fn(&OpenInput) -> bool,
    ) {
        let (due, rest): (Vec<Open>, Vec<Open>) = std::mem::take(&mut self.open_approvals)
            .into_iter()
            .partition(is_due);
        self.open_approvals = rest;
        let (due_inputs, rest): (Vec<OpenInput>, Vec<OpenInput>) =
            std::mem::take(&mut self.open_inputs)
                .into_iter()
                .partition(input_due);
        self.open_inputs = rest;
        if due.is_empty() && due_inputs.is_empty() {
            return;
        }
        self.rearm();
        for open in due {
            if let Err(error) = self
                .shared
                .db
                .resolve_approval(
                    &open.id,
                    ApprovalOutcome::Denied,
                    ResolvedBy::Expired,
                    None,
                    self.fence,
                )
                .await
            {
                tracing::error!(run = %self.run, approval = %open.id, %error, "could not record an expiry");
                self.approvals_unrecorded = true;
            }
            self.answer(
                &open.request_id,
                ApprovalOutcome::Denied,
                None,
                &ResolvedBy::Expired,
            )
            .await;
            self.publish(&ExecEvent::ApprovalResolved {
                request_id: open.request_id,
                decision: ApprovalOutcome::Denied,
                by: ResolvedBy::Expired,
                message: None,
            });
        }
        for input in due_inputs {
            // "No answers" is how every driver declines a question.
            let sent = tokio::select! {
                sent = self.handle.answer_input(input.request_id.clone(), Vec::new()) => sent,
                () = self.cancel.cancelled() => Err(reno_harness::exec::ControlsClosed),
            };
            if sent.is_err() {
                tracing::debug!(run = %self.run, "the harness was gone when its question expired");
            }
            self.publish(&ExecEvent::InputExpired {
                request_id: input.request_id,
            });
        }
        self.resolve_wait().await;
        self.refresh_view().await;
    }

    /// The run is over: close what it still has open, in the store and in the journal.
    ///
    /// When the run ended for a reason (a person's cancel, the kill switch, a budget, the
    /// shutdown), each approval records it: `withdrawn: the run was cancelled`, not the time
    /// limit it never reached.
    pub(super) async fn close_approvals(&mut self, reason: Option<&StopDetail>) {
        if self.open_approvals.is_empty() && !self.approvals_unrecorded {
            return;
        }
        self.open_approvals.clear();
        self.expiry = None;
        let scope = ExpireScope::Run {
            run: self.run.clone(),
            fence: self.fence,
        };
        let message = reason.map(|reason| format!("withdrawn: {reason}"));
        match self.shared.db.expire_pending(scope, message.clone()).await {
            Ok(expired) => {
                for closed in expired {
                    self.answer(
                        &closed.request_id,
                        ApprovalOutcome::Denied,
                        message.clone(),
                        &ResolvedBy::Expired,
                    )
                    .await;
                    self.publish(&ExecEvent::ApprovalResolved {
                        request_id: closed.request_id,
                        decision: ApprovalOutcome::Denied,
                        by: ResolvedBy::Expired,
                        message: message.clone(),
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
        let next = (self.open_approvals.iter().map(|open| open.deadline))
            .chain(self.open_inputs.iter().map(|input| input.deadline))
            .min();
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
