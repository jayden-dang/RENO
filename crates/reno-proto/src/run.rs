//! The run lifecycle (ARCHITECTURE section 4.1) as pure data.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where a run is in its lifecycle. Serialised as a snake_case string.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Waiting for a dispatcher to claim it.
    Queued,
    /// Claimed under a lease and fence; the executor is not yet running.
    Leased,
    /// The executor is working.
    Running,
    /// Paused on an `ExecEvent::NeedsInput`.
    WaitingInput,
    /// Paused on an `ExecEvent::NeedsApproval`.
    WaitingApproval,
    /// The harness finished; checks and verdicts are being gathered.
    Evaluating,
    /// Evaluation passed. Terminal.
    Succeeded,
    /// Evaluation failed or the harness errored. Terminal, except that a retry re-queues it.
    Failed,
    /// The daemon or executor stopped mid-run; resume or human review decides what follows.
    Interrupted,
    /// Cancelled before or after interruption. Terminal.
    Cancelled,
}

impl RunStatus {
    /// Every status, in lifecycle order.
    pub const ALL: [RunStatus; 10] = [
        Self::Queued,
        Self::Leased,
        Self::Running,
        Self::WaitingInput,
        Self::WaitingApproval,
        Self::Evaluating,
        Self::Succeeded,
        Self::Failed,
        Self::Interrupted,
        Self::Cancelled,
    ];

    /// True for `succeeded`, `failed` and `cancelled`: a run in one of these is not "awake".
    ///
    /// `failed` still has the retry edge to `queued` (a new attempt with `fence + 1`), which
    /// [`can_transition_to`](Self::can_transition_to) allows; no other terminal state has an
    /// outgoing edge.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    /// Whether the lifecycle diagram has an edge from `self` to `next`. Self-loops are not edges.
    pub fn can_transition_to(self, next: RunStatus) -> bool {
        use RunStatus::*;
        matches!(
            (self, next),
            (Queued, Leased | Cancelled)
                | (Leased, Running | Queued)
                | (
                    Running,
                    WaitingInput | WaitingApproval | Evaluating | Interrupted
                )
                | (WaitingInput | WaitingApproval, Running | Interrupted)
                | (Evaluating, Succeeded | Failed | Interrupted)
                | (Failed, Queued)
                | (Interrupted, Queued | Cancelled)
        )
    }
}

/// Why an executor stopped, carried by `ExecEvent::Done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The harness finished its turn normally.
    Completed,
    /// The harness was stopped before finishing.
    Interrupted,
    /// The harness failed. A failing harness ends with this, never with a stream error.
    Error,
    /// The model declined the request.
    Refusal,
    /// The model hit its output limit.
    MaxTokens,
}

#[cfg(test)]
mod tests {
    use super::RunStatus::{self, *};

    /// Every edge of the diagram. Any pair not listed here must be rejected.
    const EDGES: &[(RunStatus, RunStatus)] = &[
        (Queued, Leased),
        (Queued, Cancelled),
        (Leased, Running),
        (Leased, Queued),
        (Running, WaitingInput),
        (Running, WaitingApproval),
        (Running, Evaluating),
        (Running, Interrupted),
        (WaitingInput, Running),
        (WaitingInput, Interrupted),
        (WaitingApproval, Running),
        (WaitingApproval, Interrupted),
        (Evaluating, Succeeded),
        (Evaluating, Failed),
        // The daemon can die while checks run; recovery must be able to mark that run.
        (Evaluating, Interrupted),
        (Failed, Queued),
        (Interrupted, Queued),
        (Interrupted, Cancelled),
    ];

    #[test]
    fn transition_table_covers_every_pair() {
        for from in RunStatus::ALL {
            for to in RunStatus::ALL {
                let expected = EDGES.contains(&(from, to));
                assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn terminal_states() {
        for s in RunStatus::ALL {
            let expected = matches!(s, Succeeded | Failed | Cancelled);
            assert_eq!(s.is_terminal(), expected, "{s:?}");
        }
        for s in [Succeeded, Cancelled] {
            assert!(RunStatus::ALL.iter().all(|&n| !s.can_transition_to(n)));
        }
    }

    #[test]
    fn wire_names_are_snake_case() {
        let json = serde_json::to_string(&WaitingApproval).unwrap();
        assert_eq!(json, "\"waiting_approval\"");
    }
}
