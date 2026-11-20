//! The MOCKED thread-desk contract (ROAD-32): what a person can do with a thread from the
//! sidebar besides opening it. Rename it, pin it, archive it, and list what is archived. The real
//! daemon has no `thread.update` yet (a thread's title is its first run's prompt), so only the
//! fake daemon implements [`ThreadDesk`]; the app reads and changes it only through the
//! `Store::thread_*` entries, and only where `Capabilities::thread_edit` is on. Closing a thread
//! is real (`thread.close`) and is not here.
//!
//! The wire shape the real daemon would take: `thread.update { threadId, title?, pinned?,
//! archived? }` answers the thread, and `thread.list` takes an `archived` filter. The CLI reaches
//! the same state (ARCH-2).

use std::collections::HashMap;
use std::fmt;

use reno_proto::ThreadId;

/// What the desk keeps about threads, beyond what `thread.list` says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadMarks {
    /// A thread's own title, by thread. A thread not here is titled by its first run.
    pub titles: HashMap<ThreadId, String>,
    /// Pinned threads, the most recently pinned first.
    pub pinned: Vec<ThreadId>,
    /// Archived threads, the most recently archived first.
    pub archived: Vec<ThreadId>,
}

/// Why a thread change was not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadEditError {
    /// The new title is empty.
    NeedsTitle,
    /// A run of the thread is live: stop it first.
    LiveRun,
    /// The thread is archived: it cannot be pinned.
    Archived,
    /// The daemon does not know this thread.
    UnknownThread,
}

impl fmt::Display for ThreadEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NeedsTitle => "A thread needs a title",
            Self::LiveRun => "Stop the run before archiving",
            Self::Archived => "Unarchive the thread before pinning it",
            Self::UnknownThread => "This thread is not known to the daemon",
        })
    }
}

impl std::error::Error for ThreadEditError {}

/// The seam.
pub trait ThreadDesk: Send + Sync {
    /// Everything the desk keeps: titles, pins and the archive.
    fn marks(&self) -> ThreadMarks;
    /// The archived threads, the most recently archived first (`thread.list` with the archived
    /// filter).
    fn archived(&self) -> Vec<ThreadId>;
    /// Gives the thread a title of its own (surrounding space is trimmed) and answers the title
    /// kept. `NeedsTitle` when nothing is left.
    fn rename(&self, thread: &ThreadId, title: &str) -> Result<String, ThreadEditError>;
    /// Pins or unpins. A pinned thread goes first. Pinning twice keeps the first place.
    fn set_pinned(&self, thread: &ThreadId, pinned: bool) -> Result<(), ThreadEditError>;
    /// Archives or unarchives. Archiving unpins. `LiveRun` while a run of the thread is live.
    fn set_archived(&self, thread: &ThreadId, archived: bool) -> Result<(), ThreadEditError>;
}
