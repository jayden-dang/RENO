//! The run lifecycle (ARCHITECTURE section 4.1) as pure data.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::budget::{BudgetKind, BudgetStop};
use crate::ids::ExecutorId;

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
    /// The run in this status, in plain words for a sentence: "a cancelled run", "a run that
    /// succeeded", "a run waiting for approval".
    pub fn a_run(self) -> &'static str {
        match self {
            Self::Queued => "a queued run",
            Self::Leased => "a run that is starting",
            Self::Running => "a running run",
            Self::WaitingInput => "a run waiting for an answer",
            Self::WaitingApproval => "a run waiting for approval",
            Self::Evaluating => "a run being checked",
            Self::Succeeded => "a run that succeeded",
            Self::Failed => "a failed run",
            Self::Interrupted => "an interrupted run",
            Self::Cancelled => "a cancelled run",
        }
    }

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

/// Why a run could not start. A run that is refused never reached its harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StartRefusal {
    /// The executor's harness is not installed (or is too old to use).
    NotInstalled,
    /// The executor's harness is not signed in.
    NotSignedIn,
    /// No executor can honour the run: an access mode or sandbox level nobody offers, or a task
    /// that named no executor and has no default to go to.
    NoEligibleExecutor,
    /// The run's workspace could not be prepared: a write run with no git repository to isolate
    /// it in, or git failing.
    WorkspaceRefused,
    /// A secret the run's bot lists is not set, or the secret store cannot be read (a locked
    /// Keychain). A person has to act: `reno secret set <NAME>`.
    SecretMissing,
    /// Anything else that stopped the start: an unknown executor, an unreadable bot definition,
    /// a task with no working directory, a spec the executor rejected.
    Invalid,
}

/// Who cancelled a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CancelledBy {
    /// A person (`run.cancel`).
    User,
    /// The kill switch (`budget.pause` with `stopLive`).
    Pause,
}

/// The typed reason behind a run's last stop, beside the human `diagnostic`. A client decides
/// what to show from this and never from the diagnostic's text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum StopDetail {
    /// A budget stopped the run.
    BudgetExceeded {
        /// Which limit.
        kind: BudgetKind,
        /// The limit: milliseconds for `wall_time`, tokens for `tokens`.
        limit: u64,
        /// What had been used when it was noticed, in the same unit.
        used: u64,
    },
    /// The run never started: `failure` says why, `detail` is the daemon's words for a person.
    StartRefused {
        /// The kind of refusal.
        failure: StartRefusal,
        /// The refusal as a sentence.
        detail: String,
    },
    /// Someone cancelled the run.
    Cancelled {
        /// Who.
        by: CancelledBy,
    },
    /// The daemon was shutting down. The run is resumed at the next start when it may be.
    Shutdown,
    /// The daemon died with the run live; recovery found it at the next start.
    Crashed,
    /// The executor ended the run itself, as interrupted, without being asked to.
    ExecutorStopped,
}

impl StopDetail {
    /// The budget stop a `BudgetExceeded` carries.
    pub fn budget(&self) -> Option<BudgetStop> {
        match self {
            Self::BudgetExceeded { kind, limit, used } => Some(BudgetStop {
                kind: *kind,
                limit: *limit,
                used: *used,
            }),
            _ => None,
        }
    }

    /// Why a run that stopped for this reason is never resumed on its own, or `None` when the
    /// reason alone does not forbid it (a shutdown or a crash: the run was not asked to stop).
    pub fn forbids_auto_resume(&self) -> Option<NotResumed> {
        match self {
            Self::Cancelled {
                by: CancelledBy::User,
            } => Some(NotResumed::Cancelled),
            Self::Cancelled {
                by: CancelledBy::Pause,
            } => Some(NotResumed::Paused),
            Self::BudgetExceeded { .. } => Some(NotResumed::BudgetStopped),
            Self::ExecutorStopped => Some(NotResumed::ExecutorStopped),
            Self::StartRefused { .. } => Some(NotResumed::StartRefused),
            Self::Shutdown | Self::Crashed => None,
        }
    }
}

impl fmt::Display for StopDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BudgetExceeded { kind, limit, used } => BudgetStop {
                kind: *kind,
                limit: *limit,
                used: *used,
            }
            .fmt(f),
            Self::StartRefused { detail, .. } => f.write_str(detail),
            Self::Cancelled {
                by: CancelledBy::User,
            } => f.write_str("the run was cancelled"),
            Self::Cancelled {
                by: CancelledBy::Pause,
            } => f.write_str("the run was stopped by the kill switch"),
            Self::Shutdown => f.write_str("the daemon was shutting down"),
            Self::Crashed => f.write_str("the daemon stopped while the run was live"),
            Self::ExecutorStopped => f.write_str("the executor ended the run by itself"),
        }
    }
}

impl From<BudgetStop> for StopDetail {
    fn from(stop: BudgetStop) -> Self {
        Self::BudgetExceeded {
            kind: stop.kind,
            limit: stop.limit,
            used: stop.used,
        }
    }
}

/// Why a run was not resumed, or cannot be: set on an `interrupted` run that recovery looked at
/// and left, and the reason `run.resume` gives when it refuses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum NotResumed {
    /// A person cancelled it; their word stands.
    Cancelled,
    /// The kill switch stopped it.
    Paused,
    /// A budget stopped it; resuming would spend the same budget again.
    BudgetStopped,
    /// The executor ended it by itself; a loop of those is not retried unasked.
    ExecutorStopped,
    /// It never started on an executor.
    StartRefused,
    /// The harness never reported a session, so there is nothing to resume.
    NoSession,
    /// The run's executor does not declare resume support.
    ExecutorCannotResume {
        /// The executor.
        executor: ExecutorId,
    },
    /// The run's executor is not registered in this daemon.
    ExecutorMissing {
        /// The executor.
        executor: ExecutorId,
    },
    /// The run's bot has `resume = "never"`.
    BotForbids {
        /// The bot.
        bot: String,
    },
    /// The run has used its attempts.
    AttemptsExhausted {
        /// Attempts made.
        attempts: u32,
        /// Attempts allowed.
        max: u32,
    },
    /// The run is past its wall-time budget.
    WallTimeSpent {
        /// The limit, in milliseconds.
        limit_ms: u64,
    },
    /// The run's executor has not finished stopping; ask again in a moment.
    StillStopping,
    /// Only `interrupted` runs, and `failed` ones that have a session, can be resumed.
    WrongStatus {
        /// The run's status.
        status: RunStatus,
    },
}

impl fmt::Display for NotResumed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("it was cancelled on request"),
            Self::Paused => f.write_str("the kill switch stopped it"),
            Self::BudgetStopped => f.write_str("a budget stopped it"),
            Self::ExecutorStopped => f.write_str("its executor ended it without being asked"),
            Self::StartRefused => f.write_str("it never started"),
            Self::NoSession => f.write_str("its harness never reported a session to resume"),
            Self::ExecutorCannotResume { executor } => {
                write!(f, "executor {executor} cannot resume a session")
            }
            Self::ExecutorMissing { executor } => {
                write!(f, "executor {executor} is not available in this daemon")
            }
            Self::BotForbids { bot } => write!(f, "bot {bot} has resume = \"never\""),
            Self::AttemptsExhausted { attempts, max } => {
                write!(f, "it has made {attempts} of {max} attempts")
            }
            Self::WallTimeSpent { limit_ms } => {
                write!(f, "it is past its wall-time budget of {}s", limit_ms / 1000)
            }
            Self::StillStopping => f.write_str("its executor is still stopping"),
            Self::WrongStatus { status } => write!(
                f,
                "{} cannot be resumed; only interrupted runs, and failed runs that have a \
                 session, can",
                status.a_run()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RunStatus::{self, *};

    #[test]
    fn a_resume_refusal_names_the_run_in_plain_words() {
        let why = |status| super::NotResumed::WrongStatus { status }.to_string();
        assert_eq!(
            why(Cancelled),
            "a cancelled run cannot be resumed; only interrupted runs, and failed runs that have a session, can"
        );
        assert!(why(Succeeded).starts_with("a run that succeeded cannot be resumed"));
        assert!(why(Running).starts_with("a running run cannot be resumed"));
        assert!(why(WaitingApproval).starts_with("a run waiting for approval cannot be resumed"));
        // No debug names (`Cancelled`) and no stray capitals reach a person.
        for status in RunStatus::ALL {
            let text = why(status);
            assert!(text.starts_with("a ") || text.starts_with("an "), "{text}");
            assert!(!text.contains(&format!("{status:?}")), "{text}");
        }
    }

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
