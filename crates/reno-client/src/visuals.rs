//! The MOCKED agent-posted-visuals contract (ROAD-33): an image or a diagram an agent posts into
//! a run, shown in the transcript after the entry it was posted with. The real daemon sends none
//! of this yet, so nothing but the fake daemon implements [`VisualDesk`]; the app reads visuals
//! only through this one seam (`Store::run_visuals`). The real contract replaces the trait's
//! implementation (and, when the wire types exist, these structs); no app screen changes. No
//! protocol or daemon change lives here.

use reno_proto::{RunId, Seq};

/// Where an image visual's bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualImage {
    /// A file in the run's worktree, relative to its root, read through the daemon's file read.
    Worktree(String),
    /// The bytes themselves, with their MIME type (`image/png`).
    Data { mime: String, bytes: Vec<u8> },
}

/// What a visual shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualKind {
    Image(VisualImage),
    /// A Mermaid diagram's source.
    Diagram(String),
}

/// One image or diagram a run posted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visual {
    /// Stable within the run.
    pub id: String,
    /// Its title in the transcript.
    pub caption: String,
    /// The event of the run's journal it appears after.
    pub after: Seq,
    pub kind: VisualKind,
}

/// The seam: the visuals a run posted.
pub trait VisualDesk: Send + Sync {
    /// The run's visuals in the order they were posted (and so by `after`). Empty when the run
    /// posted none.
    fn visuals(&self, run: &RunId) -> Vec<Visual>;
}
