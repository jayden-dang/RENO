//! The MOCKED message-queue contract (ROAD-31): messages the owner wrote while a run's turn was
//! still going, waiting to be delivered when it ends. The real daemon has no queue yet (a steer is
//! delivered at the next step), so only the fake daemon implements [`MessageQueue`]; the app reads
//! and changes the queue only through the `Store::queue_*` entries. ROAD-31's real implementation
//! replaces the trait's implementation; no composer changes.

use std::fmt;

use reno_proto::RunId;

/// One waiting message. The id is stable for as long as the message waits, whatever its place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedMessage {
    pub id: String,
    pub text: String,
}

/// Why a queue change was not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// The run ended: a message to it would never be delivered. Send it as a new message.
    RunEnded,
    /// The daemon does not know this run.
    UnknownRun,
    /// The message was delivered or removed already.
    UnknownMessage,
    /// Nothing to queue.
    Empty,
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RunEnded => "The run ended; send it as a new message instead",
            Self::UnknownRun => "This run is not known to the daemon",
            Self::UnknownMessage => "That message was already sent",
            Self::Empty => "There is nothing to queue",
        })
    }
}

impl std::error::Error for QueueError {}

/// The seam: a run's queue, oldest first. The daemon delivers the first message when the turn
/// ends, then the next at the next turn end.
pub trait MessageQueue: Send + Sync {
    /// What waits for `run`, in delivery order.
    fn queue(&self, run: &RunId) -> Vec<QueuedMessage>;
    /// Adds a message at the end. `RunEnded` when the run is over.
    fn queue_add(&self, run: &RunId, text: &str) -> Result<QueuedMessage, QueueError>;
    /// Rewrites a waiting message, keeping its place and id.
    fn queue_edit(&self, run: &RunId, id: &str, text: &str) -> Result<(), QueueError>;
    /// Moves a waiting message to place `to` (0 is first; past the end is last).
    fn queue_move(&self, run: &RunId, id: &str, to: usize) -> Result<(), QueueError>;
    /// Takes a waiting message back without sending it.
    fn queue_remove(&self, run: &RunId, id: &str) -> Result<(), QueueError>;
    /// Interrupts the turn and delivers this message now; it leaves the queue.
    fn queue_send_now(&self, run: &RunId, id: &str) -> Result<QueuedMessage, QueueError>;
}
