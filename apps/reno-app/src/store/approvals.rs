//! Feedback for an approval decision until the daemon confirms its outcome.

use gpui::{AsyncApp, Context, Task};
use gpui_tokio::Tokio;
use reno_client::ConnectionStatus;
use reno_proto::{ApprovalDecision, ApprovalId, ApprovalStatus, Remember, RunId};

use super::{Store, StoreEvent, flatten};

pub(crate) enum ApprovalFeedback {
    Sending,
    /// The RPC or history confirmed the decision. Hold the actions closed until the watch catches up.
    Recorded,
    Checking(String),
    StillPending(String),
}

pub(super) struct ApprovalAttempt {
    run: RunId,
    feedback: ApprovalFeedback,
    remembering: bool,
    check: Option<Task<()>>,
}

impl Store {
    pub(crate) fn approval_feedback(&self, id: &ApprovalId) -> Option<&ApprovalFeedback> {
        self.approval_attempts
            .get(id)
            .map(|attempt| &attempt.feedback)
    }

    pub(crate) fn can_decide_approval(&self, id: &ApprovalId) -> bool {
        self.connection() == ConnectionStatus::Connected
            && self.approval_is_pending(id)
            && matches!(
                self.approval_feedback(id),
                None | Some(ApprovalFeedback::StillPending(_))
            )
    }

    pub(crate) fn approval_checking(&self, run: &RunId) -> Option<(&ApprovalId, bool)> {
        self.approval_attempts
            .iter()
            .find(|(_, attempt)| {
                &attempt.run == run && matches!(attempt.feedback, ApprovalFeedback::Checking(_))
            })
            .map(|(id, attempt)| {
                (
                    id,
                    attempt.check.is_none() && self.connection() == ConnectionStatus::Connected,
                )
            })
    }

    fn approval_is_pending(&self, id: &ApprovalId) -> bool {
        self.state
            .approvals()
            .iter()
            .any(|approval| &approval.id == id)
    }

    pub(super) fn prune_approval_feedback(&mut self) {
        let pending = self.state.approvals();
        self.approval_attempts.retain(|id, attempt| {
            !matches!(
                attempt.feedback,
                ApprovalFeedback::Recorded | ApprovalFeedback::StillPending(_)
            ) || pending.iter().any(|approval| &approval.id == id)
        });
    }

    pub(super) fn refresh_approval_attempts(&mut self, cx: &mut Context<Self>) {
        if self.connection() == ConnectionStatus::Connected {
            let checking: Vec<_> = self
                .approval_attempts
                .iter()
                .filter(|(_, attempt)| matches!(attempt.feedback, ApprovalFeedback::Checking(_)))
                .map(|(id, _)| id.clone())
                .collect();
            for id in checking {
                self.check_approval(id, cx);
            }
        } else {
            for attempt in self.approval_attempts.values_mut() {
                // A read from the previous connection cannot reopen a decision on the next one.
                attempt.check = None;
                if let ApprovalFeedback::StillPending(error) = &attempt.feedback {
                    attempt.feedback = ApprovalFeedback::Checking(error.clone());
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn decide(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        self.decide_with_message(id, decision, None, cx);
    }

    pub(crate) fn decide_with_message(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.decide_remembering(id, decision, message, None, cx);
    }

    pub(crate) fn decide_remembering(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
        remember: Option<Remember>,
        cx: &mut Context<Self>,
    ) {
        if !self.can_decide_approval(&id) {
            return;
        }
        let Some(client) = self.client() else { return };
        let Some(approval) = self
            .state
            .approvals()
            .iter()
            .find(|approval| approval.id == id)
        else {
            return;
        };
        self.approval_attempts.insert(
            id.clone(),
            ApprovalAttempt {
                run: approval.run_id.clone(),
                feedback: ApprovalFeedback::Sending,
                remembering: remember.is_some(),
                check: None,
            },
        );
        cx.notify();

        let request_id = id.clone();
        let request = Tokio::spawn(cx, async move {
            client
                .decide_remembering(&request_id, decision, message, remember)
                .await
        });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = flatten(request.await);
            this.update(cx, |store, cx| match result {
                Ok(()) => store.approval_recorded(&id, cx),
                Err(error) => {
                    if let Some(attempt) = store.approval_attempts.get_mut(&id) {
                        attempt.feedback = ApprovalFeedback::Checking(error);
                        store.check_approval(id, cx);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn check_approval(&mut self, id: ApprovalId, cx: &mut Context<Self>) {
        if self.connection() != ConnectionStatus::Connected {
            return;
        }
        let Some(client) = self.client() else { return };
        let Some(attempt) = self.approval_attempts.get_mut(&id) else {
            return;
        };
        if attempt.check.is_some() || !matches!(attempt.feedback, ApprovalFeedback::Checking(_)) {
            return;
        }
        let run = attempt.run.clone();
        let request_id = id.clone();
        let read = Tokio::spawn(cx, async move {
            let pending = client.list_approvals().await?;
            let history = client.approval_history(Some(run), None).await?;
            let recorded = history
                .into_iter()
                .find(|record| record.id == request_id && record.status != ApprovalStatus::Pending)
                .map(|record| record.status);
            let still_pending = pending.iter().any(|approval| approval.id == request_id);
            Ok((recorded, still_pending))
        });
        attempt.check = Some(cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = flatten(read.await);
            this.update(cx, |store, cx| {
                let still_visible = store.approval_is_pending(&id);
                let Some(attempt) = store.approval_attempts.get_mut(&id) else {
                    return;
                };
                attempt.check = None;
                match result {
                    Ok((Some(status), _)) => {
                        store.approval_recorded(&id, cx);
                        cx.emit(StoreEvent::ApprovalRecorded(status));
                    }
                    Ok((None, true)) if still_visible => {
                        if let ApprovalFeedback::Checking(error) = &attempt.feedback {
                            attempt.feedback = ApprovalFeedback::StillPending(error.clone());
                        }
                    }
                    _ => {} // Missing or unavailable evidence keeps the outcome uncertain.
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn approval_recorded(&mut self, id: &ApprovalId, cx: &mut Context<Self>) {
        let pending = self.approval_is_pending(id);
        let Some(attempt) = self.approval_attempts.get_mut(id) else {
            return;
        };
        let remembering = attempt.remembering;
        if pending {
            attempt.feedback = ApprovalFeedback::Recorded;
        } else {
            self.approval_attempts.remove(id);
        }
        if remembering {
            self.load_rules(cx);
        }
        cx.notify();
    }
}
